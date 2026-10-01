#!/usr/bin/env python3
"""Real host/TUI regression for streaming after catalog changes and the model workflow.

Uses a controlled local SSE provider and a measured HTTP relay. No user credentials
or external model calls. Receipts distinguish this POSIX PTY evidence from manual use.
"""

import argparse
import http.client
import http.server
import importlib.util
import json
import os
import re
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from tui_pty import Terminal

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("live_tui", ROOT / "scripts/verify-tui.py")
assert SPEC and SPEC.loader
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


class Provider(http.server.ThreadingHTTPServer):
    """Record only fixture requests; coordinate cancellation at a pending stream."""

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.sent = {}
        self.requests = []
        self.started = threading.Event()
        self.allow = threading.Event()
        self.done = threading.Event()
        self.disconnected = threading.Event()
        self.mode = "stream"
        self.tool_round = 0


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_POST(self):
        server = self.server
        assert isinstance(server, Provider)
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        server.requests.append(request)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        server.started.set()

        def emit(delta, finish=None, usage=None):
            value = {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            if usage:
                value["usage"] = usage
            self.wfile.write(("data: " + json.dumps(value) + "\n\n").encode())
            self.wfile.flush()

        try:
            if server.mode in {"tools", "blocking_tool"}:
                calls = [
                    ("write", {"path": "sample.txt", "content": "before\nsecond\n"}),
                    ("read", {"path": "sample.txt"}),
                    ("edit", {"path": "sample.txt", "old_text": "before", "new_text": "after"}),
                    ("bash", {"command": "printf TOOL_OUTPUT"}),
                    (
                        "bash",
                        {
                            "command": "i=0; while [ $i -lt 2000 ]; do echo LONG_TOOL_OUTPUT_$i; i=$((i+1)); done"
                        },
                    ),
                    ("bash", {"command": "printf TOOL_FAILURE >&2; exit 7"}),
                ]
                if server.mode == "blocking_tool":
                    calls = [
                        (
                            "bash",
                            {
                                "command": "echo $$ > parent.pid; sleep 60 & echo $! > child.pid; wait"
                            },
                        )
                    ]
                index = server.tool_round
                server.tool_round += 1
                if index < len(calls):
                    name, arguments = calls[index]
                    emit(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": f"call-{len(server.requests)}-{index}",
                                    "type": "function",
                                    "function": {"name": name, "arguments": json.dumps(arguments)},
                                }
                            ]
                        },
                        "tool_calls",
                    )
                else:
                    emit({"content": "TOOLS_FINISHED"}, "stop")
            elif server.mode == "single":
                server.allow.wait(15)
                emit({"content": "STREAM_FINISHED"}, "stop")
            else:
                server.allow.wait(15)
                emit({"content": "```rust\n"})
                for index in range(600 if server.mode == "stream" else 2):
                    server.sent[index] = time.monotonic()
                    emit(
                        {
                            "content": f'let item_{index:05d} = format!("sample {{}}", value); // 中文 streaming\n'
                        }
                    )
                    time.sleep(0.01)
                emit(
                    {"content": "\n```\nSTREAM_FINISHED"},
                    "stop",
                    {"prompt_tokens": 1234, "completion_tokens": 5678},
                )
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            server.disconnected.set()
        finally:
            server.done.set()


class Relay(http.server.ThreadingHTTPServer):
    def __init__(self, address):
        super().__init__(("127.0.0.1", 0), RelayHandler)
        self.address = address
        self.snapshot_bytes = 0
        self.snapshot_count = 0


class RelayHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        relay = self.server
        assert isinstance(relay, Relay)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        connection = http.client.HTTPConnection(relay.address, timeout=30)
        try:
            connection.request(
                self.command,
                self.path,
                body=body,
                headers={"X-Eden-Token": self.headers["X-Eden-Token"]},
            )
            response = connection.getresponse()
            data = response.read()
            if self.path.startswith("/tui/snapshot"):
                relay.snapshot_bytes += len(data)
                relay.snapshot_count += 1
            self.send_response(response.status)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            connection.close()


def rss(pid):
    path = Path(f"/proc/{pid}/status")
    if path.exists():
        match = re.search(r"VmRSS:\s+(\d+)", path.read_text())
        return int(match[1]) if match else None
    return None


def run(args, mutations):
    server = Provider()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    terminal = None
    relay = None
    host = None
    endpoint = None
    with tempfile.TemporaryDirectory(prefix="eden-tui-streaming-") as temporary:
        root = Path(temporary)
        try:
            config = json.loads(args.composition.read_text())
            for package in config["packages"]:
                package["library"] = str(args.binary.parent / Path(package["library"]).name)
                if package["descriptor"]["package"] == "model-access":
                    package["config"] = {
                        "catalog": {
                            "models": [
                                {
                                    "provider": "fixture",
                                    "model": name,
                                    "api": "openai-completions",
                                    "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                                    "limits": {
                                        "context_window": 262144,
                                        "max_output_tokens": 32768,
                                    },
                                    "capabilities": {
                                        "tools": True,
                                        "images": False,
                                        "reasoning": False,
                                    },
                                    "source": {"kind": "author", "location": "local regression"},
                                }
                                for name in ["audit-A", "audit-B"]
                            ]
                        },
                        "credentials": {
                            "path": str(root / "global/keys.json"),
                        },
                    }
            composition = root / "composition.json"
            composition.write_text(json.dumps(config))
            endpoint_file = root / "endpoint.json"
            command = [
                str(args.binary),
                "--composition",
                str(composition),
                "--cwd",
                str(root),
                "--global-dir",
                str(root / "global"),
                "--session",
                str(root / "history.jsonl"),
                "--offline-startup",
                "--no-trust-project",
                "live",
                "--endpoint",
                str(endpoint_file),
            ]
            host = subprocess.Popen(
                command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            endpoint = live.ready(host, endpoint_file)
            counter = 0

            def call(route, body=None):
                return live.call(endpoint, route, body)

            def mutate(route, body):
                nonlocal counter
                counter += 1
                reply = call(route, {**body, "request_id": f"fixture-{counter}"})
                if "run_id" in reply:
                    terminal_result = call("/terminal", {"run_id": reply["run_id"]})
                    assert terminal_result["outcome"]["status"] == "completed", terminal_result
                    return terminal_result
                return reply

            target = {"provider": "fixture", "model": "audit-A", "thinking": "off"}
            relay = Relay(endpoint["address"])
            threading.Thread(target=relay.serve_forever, daemon=True).start()
            relay_file = root / "relay.json"
            relay_file.write_text(
                json.dumps({**endpoint, "address": f"127.0.0.1:{relay.server_port}"})
            )
            terminal = Terminal(
                args.binary,
                relay_file,
                env={"EDEN_TUI_EDITOR": str(args.editor)},
                width=120,
                height=36,
            )
            terminal.wait("Connected")
            terminal.command("/auth")
            terminal.wait("Authentication")
            terminal.send(b"fixture\r")
            terminal.wait("API key")
            terminal.send(b"\x13")
            terminal.wait("Enter private key")
            terminal.send(b"Enter private\r")
            terminal.wait("Private authentication input")
            terminal.send(b"fixture-key\x13")
            terminal.wait("Operation result")
            args.output.mkdir(parents=True, exist_ok=True)
            (args.output / f"auth-{mutations}.txt").write_text(terminal.display)
            assert "fixture-key" not in json.dumps(call("/tui/snapshot"))
            terminal.send(b"\x1b")
            terminal.send(b"\x1b")
            terminal.wait("Ask anything")
            mutate("/models/select", {"selection": target})
            for _ in range(mutations):
                mutate(
                    "/models/catalog", {"request": {"action": "set_default", "selection": target}}
                )
            baseline_bytes = relay.snapshot_bytes
            run_id = call(
                "/prompt", {"request_id": "stream", "text": "Stream the controlled sample."}
            )["run_id"]
            assert server.started.wait(8), call("/terminal", {"run_id": run_id})
            server.allow.set()
            frames = []
            previous = -1
            input_start = None
            input_ms = None
            peak_host = rss(host.pid)
            peak_tui = rss(terminal.process.pid)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                terminal.read(0.003)
                matches = re.findall(r"item_(\d{5})", terminal.display)
                if matches:
                    index = max(map(int, matches))
                    if index > previous:
                        now = time.monotonic()
                        frames.append((now, index, (now - server.sent[index]) * 1000))
                        previous = index
                if len(server.sent) >= 200 and input_start is None:
                    input_start = time.monotonic()
                    os.write(terminal.master, b"INPUT_MARKER")
                if input_start and input_ms is None and "› INPUT_MARKER" in terminal.display:
                    input_ms = (time.monotonic() - input_start) * 1000
                peak_host = max(peak_host or 0, rss(host.pid) or 0)
                peak_tui = max(peak_tui or 0, rss(terminal.process.pid) or 0)
                if server.done.is_set() and "STREAM_FINISHED" in terminal.display:
                    break
            result = call("/terminal", {"run_id": run_id})
            assert result["outcome"]["status"] == "completed", result
            intervals = [(b[0] - a[0]) * 1000 for a, b in zip(frames, frames[1:], strict=False)]
            snapshot = call("/tui/snapshot")
            catalog_terminals = [
                r
                for r in snapshot["history"]
                if r["kind"] == "terminal"
                and "models" in r["payload"].get("outcome", {}).get("value", {})
            ]
            assert not catalog_terminals, "Catalog listings were persisted as terminal history"
            receipt = {
                "evidence": "controlled SSE + real host + POSIX PTY",
                "catalog_mutations": mutations,
                "chunks": len(server.sent),
                "observed_distinct_text_frames": len(frames),
                "frame_interval_p50_ms": statistics.median(intervals),
                "frame_interval_max_ms": max(intervals),
                "latest_visible_chunk_lag_p50_ms": statistics.median(f[2] for f in frames),
                "input_ms": input_ms,
                "stream_wire_bytes": relay.snapshot_bytes - baseline_bytes,
                "snapshot_wire_bytes": len(json.dumps(snapshot).encode()),
                "host_peak_rss_kib": peak_host,
                "tui_peak_rss_kib": peak_tui,
            }
            args.output.mkdir(parents=True, exist_ok=True)
            (args.output / f"stream-{mutations}.txt").write_text(terminal.display)
            (args.output / f"stream-{mutations}.json").write_text(
                json.dumps(receipt, indent=2) + "\n"
            )
            assert len(server.sent) == 600 and previous == 599, receipt
            assert receipt["frame_interval_p50_ms"] < 100, receipt
            terminal.wait("5678")
            deadline = time.monotonic() + 8
            while "Working…" in terminal.display:
                terminal.read(0.01)
                assert time.monotonic() < deadline, terminal.display
            terminal.send(b"\x03")
            terminal.wait("Ask anything")
            server.mode = "short"
            server.started.clear()
            server.allow.clear()
            server.done.clear()
            run_id = call("/prompt", {"request_id": "cancel", "text": "Wait for cancellation."})[
                "run_id"
            ]
            assert server.started.wait(8)
            terminal.wait("Working")
            started = time.monotonic()
            os.write(terminal.master, b"\x03")
            until = time.monotonic() + 8
            while not any(
                label in terminal.display
                for label in ["Cancellation", "Cancelling", "Cancelled", "Run cancelled"]
            ):
                terminal.read(0.002)
                assert time.monotonic() < until, terminal.display
            receipt["cancel_feedback_ms"] = (time.monotonic() - started) * 1000
            cancelled = call("/terminal", {"run_id": run_id})
            receipt["cancel_settled_ms"] = (time.monotonic() - started) * 1000
            assert (
                cancelled["outcome"]["status"] == "cancelled" and not cancelled["cleanup_errors"]
            ), cancelled
            server.allow.set()
            assert server.done.wait(8)
            server.mode = "tools"
            terminal.command("Continue with real tools.")
            terminal.wait("TOOLS_FINISHED", seconds=20)
            snapshot = call("/tui/snapshot")
            assert (root / "sample.txt").read_text() == "after\nsecond\n"
            records = snapshot["history"]
            assert sum(r["payload"].get("type") == "tool_result" for r in records) == 6
            (args.output / f"tools-{mutations}.txt").write_text(terminal.display)
            (args.output / f"tools-{mutations}.ansi").write_bytes(terminal.output)
            terminal.send(b"\x1b[5~")
            before_scroll = terminal.display
            assert before_scroll != (args.output / f"tools-{mutations}.txt").read_text(), (
                "PageUp did not move the real transcript"
            )
            (args.output / f"scrollback-{mutations}.txt").write_text(before_scroll)
            terminal.send(b"\x1b[1;5F")
            terminal.resize(64, 24)
            terminal.wait("TOOLS_FINISHED")
            (args.output / f"narrow-{mutations}.txt").write_text(terminal.display)
            terminal.resize(120, 36)
            terminal.wait("TOOLS_FINISHED")
            terminal.command("/models")
            terminal.wait("Models")
            terminal.send(b"audit-B\r")
            terminal.wait("Save as global default")
            terminal.send(b"\t \x13")
            terminal.wait("Operation result")
            selected = call("/models/current", {})
            assert selected["selection"]["model"] == "audit-B", selected
            assert (
                json.loads((root / "global/model-catalog.json").read_text())["default"]["model"]
                == "audit-B"
            )
            terminal.send(b"\x1b")
            terminal.send(b"\x1b")
            terminal.wait("Ask anything")
            server.mode = "short"
            server.started.clear()
            terminal.command("Use selected model.")
            assert server.started.wait(8)
            terminal.wait("STREAM_FINISHED")
            active = call("/tui/snapshot")["state"]["active_run"]
            if active:
                call("/terminal", {"run_id": active})
            assert server.requests[-1]["model"] == "audit-B"
            server.mode = "blocking_tool"
            server.tool_round = 0
            terminal.command("Run the cancellable shell tool.")
            deadline = time.monotonic() + 10
            while not (root / "child.pid").exists():
                terminal.read(0.02)
                assert time.monotonic() < deadline, terminal.display
            pids = [int((root / name).read_text()) for name in ["parent.pid", "child.pid"]]
            shell_run = call("/tui/snapshot")["state"]["active_run"]
            assert shell_run is not None
            terminal.send(b"\x03")
            terminal_result = call("/terminal", {"run_id": shell_run})
            assert (
                terminal_result["outcome"]["status"] == "cancelled"
                and not terminal_result["cleanup_errors"]
            ), terminal_result
            for pid in pids:
                status = Path(f"/proc/{pid}/stat")
                assert not status.exists() or status.read_text().split()[2] == "Z", (
                    f"tool process still running: {pid}"
                )
            receipt["tool_process_tree_cancelled"] = True
            server.mode = "short"
            terminal.close()
            terminal = None
            live.stop(host, endpoint)
            command[command.index("live")] = "resume-live"
            host = subprocess.Popen(
                command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            endpoint = live.ready(host, endpoint_file)
            reopened = call("/tui/snapshot")
            assert "TOOLS_FINISHED" in json.dumps(reopened["history"])
            assert call("/models/current", {})["selection"]["model"] == "audit-B"
            live.stop(host, endpoint)
            command[command.index("resume-live")] = "live"
            command[command.index(str(root / "history.jsonl"))] = str(root / "cold.jsonl")
            host = subprocess.Popen(
                command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            endpoint = live.ready(host, endpoint_file)
            terminal = Terminal(
                args.binary,
                endpoint_file,
                env={"EDEN_TUI_EDITOR": str(args.editor)},
                width=120,
                height=36,
            )
            terminal.wait("fixture/audit-B")
            terminal.wait("global default")
            server.started.clear()
            terminal.command("Cold start uses the saved default.")
            assert server.started.wait(8)
            terminal.wait("STREAM_FINISHED")
            assert server.requests[-1]["model"] == "audit-B"
            receipt["authentication_cancel_continue_tools_model_default_reopen_cold_start"] = (
                "passed"
            )
            (args.output / f"stream-{mutations}.json").write_text(
                json.dumps(receipt, indent=2) + "\n"
            )
            print(json.dumps(receipt), flush=True)
        finally:
            server.allow.set()
            if terminal:
                terminal.close()
            if host and endpoint:
                live.stop(host, endpoint)
            if relay:
                relay.shutdown()
                relay.server_close()
            server.shutdown()
            server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/eden")
    parser.add_argument(
        "--composition", type=Path, default=ROOT / "artifacts/install-tui/composition.json"
    )
    parser.add_argument(
        "--editor", type=Path, default=ROOT / "target/release/libeden_terminal_editor.so"
    )
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/tui-streaming")
    args = parser.parse_args()
    for mutations in [0, 3]:
        run(args, mutations)


if __name__ == "__main__":
    main()
