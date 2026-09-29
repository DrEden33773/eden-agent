#!/usr/bin/env python3
"""Real Eden host and POSIX PTY probe for the whole-pager transplant."""

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

spec = importlib.util.spec_from_file_location("streaming", ROOT / "scripts/verify-tui-streaming.py")
assert spec and spec.loader
streaming = importlib.util.module_from_spec(spec)
spec.loader.exec_module(streaming)
Terminal = streaming.Terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    candidate = ROOT / "artifacts/r1-candidate"
    binary = candidate / "bin/eden"
    server = streaming.Provider()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    server.allow.set()
    terminal = None
    host = None
    endpoint = None
    checks = {}
    with tempfile.TemporaryDirectory(prefix="eden-grok-port-") as temporary:
        root = Path(temporary)
        try:
            config = json.loads((candidate / "composition.json").read_text())
            for package in config["packages"]:
                package["library"] = str((candidate / package["library"]).resolve())
                if package["descriptor"]["package"] == "model-access":
                    package["config"] = {
                        "catalog": {
                            "models": [
                                {
                                    "provider": "fixture",
                                    "model": "audit-A",
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
                                    "source": {
                                        "kind": "author",
                                        "location": "local Grok port probe",
                                    },
                                }
                            ]
                        },
                        "credentials": {"path": str(root / "global/keys.json")},
                    }
            composition = root / "composition.json"
            composition.write_text(json.dumps(config))
            endpoint_file = root / "endpoint.json"
            command = [
                str(binary),
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
            endpoint = streaming.live.ready(host, endpoint_file)

            def call(route, body=None):
                return streaming.live.call(endpoint, route, body)

            def mutate(route, body):
                reply = call(route, {**body, "request_id": f"probe-{time.monotonic_ns()}"})
                terminal_result = call("/terminal", {"run_id": reply["run_id"]})
                assert terminal_result["outcome"]["status"] == "completed", terminal_result
                return terminal_result["outcome"]["value"]

            challenge = mutate(
                "/auth/start", {"request": {"action": "start", "provider": "fixture"}}
            )
            reply = call(
                "/auth/input",
                {
                    "operation_id": challenge["operation_id"],
                    "api_key": True,
                    "input": "fixture-key",
                },
            )
            call("/terminal", reply)
            mutate(
                "/models/select",
                {"selection": {"provider": "fixture", "model": "audit-A", "thinking": "off"}},
            )
            terminal = Terminal(HERE / "run.py", endpoint_file, width=110, height=40)

            def capture(name):
                assert terminal is not None
                terminal.read(0.2)
                (args.output / f"{name}.txt").write_text(terminal.display)
                ansi = args.output / f"{name}.ansi"
                ansi.write_bytes(terminal.output)
                decoder = ROOT / "artifacts/g1-grok-port/eden-decode"
                if decoder.exists():
                    screen = json.loads(
                        subprocess.check_output(
                            [str(decoder), str(ansi), str(terminal.width), str(terminal.height)]
                        )
                    )
                    (args.output / f"{name}.json").write_text(
                        json.dumps(screen, ensure_ascii=False, indent=2)
                    )
                    (args.output / f"{name}.txt").write_text("\n".join(screen["screen"]["lines"]))

            terminal.wait("audit-A", seconds=30)
            capture("startup")
            terminal.send(b"/eden-")
            terminal.wait("eden-status")
            capture("completion")
            terminal.send(b"\t")
            terminal.wait("/eden-status")
            terminal.send(b"\x15")
            checks["input_completion"] = True
            terminal.command("hello scripted tui")
            terminal.wait("item_00010", seconds=30)
            capture("stream")
            terminal.send(b"\x03")
            frame = streaming.live.wait_for(endpoint, lambda s: s["state"]["active_run"] is None)
            outcome = next(
                r["payload"] for r in reversed(frame["history"]) if r["kind"] == "terminal"
            )
            assert outcome["outcome"]["status"] == "cancelled", outcome
            assert not outcome["cleanup_errors"]
            terminal.wait("cancelled", seconds=15)
            checks["ctrl_c_cancelled"] = True
            capture("cancel")
            server.mode = "tools"
            server.tool_round = 0
            terminal.command("run real tools")
            terminal.wait("TOOLS_FINISHED", seconds=45)
            terminal.resize(110, 45)
            capture("tools")
            assert (root / "sample.txt").read_text() == "after\nsecond\n"
            frame = call("/tui/snapshot")
            results = [r for r in frame["history"] if r["payload"].get("type") == "tool_result"]
            assert len(results) == 6, len(results)
            checks["real_tools"] = len(results)
            terminal.resize(64, 24)
            capture("narrow")
            terminal.resize(110, 40)
            terminal.send(b"\x1b[5~")
            capture("scrollback")
            terminal.send(b"\x03\x03")
            terminal.read(1)
            if terminal.process.poll() is None:
                terminal.command("/quit")
            terminal.close()
            terminal = None
            checks["terminal_restored"] = True
            streaming.live.stop(host, endpoint)
            host = None
            command[command.index("live")] = "resume-live"
            host = subprocess.Popen(
                command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            endpoint = streaming.live.ready(host, endpoint_file)
            frame = call("/tui/snapshot")
            checks["reopened_history"] = len(frame["history"])
            terminal = Terminal(HERE / "run.py", endpoint_file, width=110, height=40)
            terminal.wait("audit-A", seconds=30)
            terminal.wait("TOOLS_FINISHED", seconds=20)
            capture("reopen")
            terminal.send(b"\x1b[5~")
            terminal.wait("item_000", seconds=10)
            capture("reopen-partial")
            checks["replayed_cancelled_text"] = True
            terminal.send(b"\x1b[6~")
            checks["replayed_tools"] = True
            assert len(server.requests) == 8, len(server.requests)
            checks["reopen_did_not_execute"] = True
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
            result = call("/terminal", {"run_id": shell_run})
            assert result["outcome"]["status"] == "cancelled", result
            assert not result["cleanup_errors"]
            for pid in pids:
                status = Path(f"/proc/{pid}/stat")
                assert not status.exists() or status.read_text().split()[2] == "Z"
            checks["cancelled_process_tree"] = True
            capture("tool-cancel")
        finally:
            if terminal is not None:
                (args.output / "last.txt").write_text(terminal.display)
                (args.output / "last.ansi").write_bytes(terminal.output)
                if terminal.process.poll() is None:
                    terminal.send(b"\x03\x03")
                    terminal.read(1)
                try:
                    terminal.close()
                except (AssertionError, subprocess.TimeoutExpired):
                    terminal.process.kill()
                    terminal.process.wait()
            if host is not None and endpoint is not None:
                streaming.live.stop(host, endpoint)
            server.shutdown()
            (args.output / "checks.json").write_text(json.dumps(checks, indent=2) + "\n")


if __name__ == "__main__":
    main()
