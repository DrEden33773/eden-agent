#!/usr/bin/env python3
"""Real Eden host and POSIX PTY probe for the whole-pager transplant."""

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from management_probe import ModelRelay

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

spec = importlib.util.spec_from_file_location("streaming", ROOT / "scripts/verify-tui-streaming.py")
assert spec and spec.loader
streaming = importlib.util.module_from_spec(spec)
spec.loader.exec_module(streaming)


class ProviderHandler(streaming.Handler):
    def do_POST(self):
        if self.server.mode != "reject":
            return super().do_POST()
        self.server.requests.append(
            json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        )
        payload = json.dumps(
            {"error": {"message": "Injected provider rejection", "type": "authentication_error"}}
        ).encode()
        self.send_response(401)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class Terminal(streaming.Terminal):
    """Answer the version query so startup Esc is not a tentative DCS fragment."""

    def paint(self, data):
        if "\x1b[>0q" in self.pending + data and not getattr(self, "xt_answered", False):
            os.write(self.master, b"\x1bP>|EdenProbe 1.0\x1b\\")
            self.xt_answered = True
        super().paint(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/g1-native-host")
    parser.add_argument("--management", action="store_true")
    parser.add_argument("--draft-cancel", action="store_true")
    parser.add_argument("--busy-send", action="store_true")
    parser.add_argument("--ack-lifecycle", action="store_true")
    parser.add_argument("--thinking", action="store_true")
    args = parser.parse_args()
    if args.busy_send and not args.draft_cancel:
        parser.error("--busy-send requires --draft-cancel")
    args.output.mkdir(parents=True, exist_ok=True)
    candidate = args.installation.resolve()
    binary = candidate / "bin/eden"
    server = streaming.Provider()
    server.RequestHandlerClass = ProviderHandler
    threading.Thread(target=server.serve_forever, daemon=True).start()
    server.allow.set()
    terminal = None
    host = None
    endpoint = None
    relay = None
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
            if args.thinking:
                for package in config["packages"]:
                    if package["descriptor"]["package"] == "model-access":
                        model = package["config"]["catalog"]["models"][0]
                        model["capabilities"]["reasoning"] = True
                        model["compat"] = {"thinkingLevelMap": {"minimal": None, "max": "max"}}
            if args.management:
                for package in config["packages"]:
                    if package["descriptor"]["package"] == "model-access":
                        other = dict(package["config"]["catalog"]["models"][0])
                        other["model"] = "audit-B"
                        package["config"]["catalog"]["models"].append(other)
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
                command,
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "EDEN_TUI_STATE_DIR": str(root / "host-state")},
            )
            endpoint = streaming.live.ready(host, endpoint_file)
            relay = ModelRelay(endpoint["address"])
            threading.Thread(target=relay.serve_forever, daemon=True).start()
            relay_file = root / "relay.json"
            relay_file.write_text(
                json.dumps({**endpoint, "address": f"127.0.0.1:{relay.server_port}"})
            )

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
            if args.ack_lifecycle:
                os.environ["GROK_PROMPT_ACK_TIMEOUT_SECS"] = "5"
            terminal = Terminal(HERE / "run.py", relay_file, width=110, height=40)

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
            if args.thinking:
                for requested, effective in [("max", "max"), ("off", "off"), ("minimal", "low")]:
                    terminal.command(f"/effort {requested}")
                    deadline = time.monotonic() + 5
                    thinking = None
                    while time.monotonic() < deadline:
                        thinking = call("/models/current", {})["effective_target"]["thinking"]
                        if thinking == {"requested": requested, "effective": effective}:
                            break
                        terminal.read(0.1)
                    assert thinking == {"requested": requested, "effective": effective}, (
                        requested,
                        thinking,
                        terminal.display,
                    )
                    terminal.wait(
                        f"({requested})" if requested == effective else f"{requested} → {effective}"
                    )
                    capture(f"thinking-{requested}")
                checks["thinking_full_levels_and_sparse_mapping"] = True
            assert "Shift+Tab:mode" not in terminal.display.replace(" ", "")
            if args.management:
                terminal.send(b"\x10voice")
                terminal.read(0.3)
                assert "/voice" not in terminal.display
                capture("command-palette")
                for _ in range(3):
                    if "┌─ Commands" not in terminal.display:
                        break
                    terminal.send(b"\x1b")
                assert "┌─ Commands" not in terminal.display, "palette did not close"
                terminal.command("/voice")
                terminal.wait("not provided by the bundled Eden backend")
                assert not server.requests
                checks["unavailable_command_not_offered_or_sent"] = True
            terminal.send(b"/eden-")
            terminal.wait("eden-status")
            capture("completion")
            terminal.send(b"\t")
            terminal.wait("/eden-status")
            terminal.send(b"\x15")
            checks["input_completion"] = True
            if args.ack_lifecycle:
                server.allow.clear()
            terminal.command("hello scripted tui")
            if args.ack_lifecycle:
                assert server.started.wait(10), "provider was not reached"
                terminal.read(6)
                state = call("/tui/snapshot")["state"]
                assert state["active_run"] is not None, (
                    "accepted prompt cancelled before the first provider event",
                    state,
                )
                assert "not acknowledge" not in terminal.display
                checks["accepted_before_first_event"] = True
                server.allow.set()
            terminal.wait("item_00010", seconds=30)
            capture("stream")
            if args.draft_cancel:
                terminal.send(b"DRAFT_KEEP")
                terminal.wait("DRAFT_KEEP")
                if args.busy_send:
                    terminal.send(b"\r")
                    assert any(
                        "│ ❯ DRAFT_KEEP" in line for line in terminal.display.splitlines()[-8:]
                    ), "busy send did not keep the composer draft"
                    assert len(server.requests) == 1
                    checks["busy_send_preserves_draft"] = True
            terminal.send(b"\x03")
            frame = streaming.live.wait_for(endpoint, lambda s: s["state"]["active_run"] is None)
            outcome = next(
                r["payload"] for r in reversed(frame["history"]) if r["kind"] == "terminal"
            )
            assert outcome["outcome"]["status"] == "cancelled", outcome["outcome"]["status"]
            assert not outcome["cleanup_errors"]
            terminal.wait("cancelled", seconds=15)
            checks["ctrl_c_cancelled"] = True
            if args.draft_cancel:
                assert "DRAFT_KEEP" in terminal.display
                checks["cancel_preserves_draft"] = True
                capture("cancel-with-draft")
                terminal.send(b"\x15")
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
                command,
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "EDEN_TUI_STATE_DIR": str(root / "host-state")},
            )
            endpoint = streaming.live.ready(host, endpoint_file)
            relay.address = endpoint["address"]
            relay_file.write_text(
                json.dumps({**endpoint, "address": f"127.0.0.1:{relay.server_port}"})
            )
            frame = call("/tui/snapshot")
            checks["reopened_history"] = len(frame["history"])
            if args.ack_lifecycle:
                os.environ["GROK_PROMPT_ACK_TIMEOUT_SECS"] = "5"
            terminal = Terminal(HERE / "run.py", relay_file, width=110, height=40)
            terminal.wait("audit-A", seconds=30)
            terminal.wait("TOOLS_FINISHED", seconds=20)
            if args.thinking:
                assert call("/models/current", {})["effective_target"]["thinking"] == {
                    "requested": "minimal",
                    "effective": "low",
                }
                terminal.wait("minimal → low")
                checks["thinking_reopens_requested_and_effective"] = True
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
            if args.management:
                terminal.command(
                    "/model fixture/audit-B minimal"
                    if args.thinking
                    else "/model fixture/audit-B off"
                )
                streaming.live.wait_for(
                    endpoint,
                    lambda frame: any(
                        r["kind"] == "model_selection"
                        and r["payload"]["target"]["model"] == "audit-B"
                        for r in frame["history"]
                    ),
                )
                assert call("/models/current", {})["effective_target"]["model"] == "audit-B"
                catalog_path = root / "global/model-catalog.json"
                assert (
                    not catalog_path.exists()
                    or json.loads(catalog_path.read_text()).get("default", {}).get("model")
                    != "audit-B"
                )
                terminal.wait("audit-B")
                if args.thinking:
                    assert call("/models/current", {})["effective_target"]["thinking"] == {
                        "requested": "minimal",
                        "effective": "low",
                    }
                checks["model_current_only"] = True
                capture("model-current")
                terminal.command("/settings")
                terminal.wait("Settings")
                terminal.send(b"/Use & save model\r\r")
                terminal.wait("audit-A")
                capture("settings-model")
                terminal.send(b"\x1b[A\r")
                terminal.wait("Current model and new-session default saved", seconds=20)
                assert call("/models/current", {})["effective_target"]["model"] == "audit-A"
                if args.thinking:
                    assert json.loads(catalog_path.read_text())["default"]["thinking"] == "minimal"
                assert json.loads(catalog_path.read_text())["default"]["model"] == "audit-A"
                checks["model_current_and_default"] = True
                capture("settings-saved")
                relay.fail_defaults = True
                terminal.send(b"\r\x1b[B\r")
                terminal.wait("Current model changed; default was not saved.", seconds=20)
                assert call("/models/current", {})["effective_target"]["model"] == "audit-B"
                assert json.loads(catalog_path.read_text())["default"]["model"] == "audit-A"
                checks["default_failure_keeps_actual_current_model"] = True
                capture("settings-partial-failure")
                relay.fail_defaults = False
                terminal.send(b"\x1b\x1b")
                server.mode = "short"
                terminal.command("Report usage now.")
                terminal.wait("STREAM_FINISHED", seconds=20)
                terminal.command("/usage")
                terminal.wait("1234", seconds=15)
                terminal.wait("5678", seconds=15)
                capture("usage")
                checks["reported_usage"] = True
                before_shell = len(server.requests)
                terminal.command("!printf NATIVE_USER_SHELL")
                terminal.wait("NATIVE_USER_SHELL", seconds=15)
                terminal.read(0.5)
                frame = streaming.live.wait_for(
                    endpoint,
                    lambda frame: any(
                        r["kind"] == "user_shell"
                        and "NATIVE_USER_SHELL" in r["payload"].get("command", "")
                        for r in frame["history"]
                    ),
                )
                assert len(server.requests) == before_shell
                checks["user_shell_bypasses_model"] = True
                capture("user-shell")
                mutate("/manage/metadata", {"name": "Native saved marker", "tags": []})
                saved_dir = root / ".eden/sessions"
                saved_dir.mkdir(parents=True, exist_ok=True)
                saved_history = saved_dir / "saved.jsonl"
                shutil.copyfile(root / "history.jsonl", saved_history)
                mutate("/manage/metadata", {"name": "Native current marker", "tags": []})
                terminal.command("/resume")
                terminal.wait("Native saved marker", seconds=20)
                capture("session-picker")
                terminal.send(b"Native saved marker")
                terminal.read(0.5)
                terminal.send(b"\r")
                terminal.wait("STREAM_FINISHED", seconds=20)
                capture("session-switched")
                checks["session_picker_opened_history"] = True
                assert len(server.requests) == before_shell
                checks["session_switch_did_not_execute"] = True
                assert relay.opened, "picker never opened an Eden endpoint"
                child = json.loads(relay.opened[-1].read_text())
                terminal.command(
                    "/model fixture/audit-A minimal"
                    if args.thinking
                    else "/model fixture/audit-A off"
                )
                terminal.wait("audit-A")
                deadline = time.monotonic() + 10
                while (
                    streaming.live.call(child, "/models/current", {})["effective_target"]["model"]
                    != "audit-A"
                ):
                    terminal.read(0.05)
                    assert time.monotonic() < deadline
                assert call("/models/current", {})["effective_target"]["model"] == "audit-B"
                checks["session_switch_routes_actions_to_selected_host"] = True
                capture("session-selected-model")
                server.mode = "reject"
                terminal.command("PROVIDER_FAILURE_MARKER")
                failed_frame = streaming.live.wait_for(
                    child,
                    lambda frame: frame["state"]["active_run"] is None
                    and any(
                        r["kind"] == "terminal" and r["payload"]["outcome"]["status"] == "failed"
                        for r in frame["history"]
                    ),
                )
                diagnostic = next(
                    r["payload"]["outcome"]["value"]["message"]
                    for r in reversed(failed_frame["history"])
                    if r["kind"] == "terminal" and r["payload"]["outcome"]["status"] == "failed"
                )
                assert "401" in diagnostic
                terminal.wait(diagnostic, seconds=15)
                checks["provider_failure_preserves_diagnostic"] = True
                capture("provider-failure")
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
            for candidate_endpoint in (root / "host-state").rglob("*.json"):
                try:
                    data = json.loads(candidate_endpoint.read_text())
                    if isinstance(data, dict) and {"address", "token", "session_id"} <= data.keys():
                        streaming.live.call(data, "/shutdown", {})
                except (OSError, RuntimeError, ValueError):
                    pass
            if relay is not None:
                relay.shutdown()
            server.shutdown()
            (args.output / "checks.json").write_text(json.dumps(checks, indent=2) + "\n")


if __name__ == "__main__":
    main()
