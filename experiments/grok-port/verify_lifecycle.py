#!/usr/bin/env python3
"""Prompt/run/attempt regression through the native adapter, real host and original Grok PTY."""

import argparse
import http.client
import http.server
import json
import threading
import time
from pathlib import Path
from typing import Any, Protocol, cast

from verify_workflows import ROOT, Fixture, Rpc


class ProviderState(Protocol):
    requests: list[dict[str, Any]]
    mode: str
    allow: threading.Event
    started: threading.Event
    disconnected: threading.Event
    done: threading.Event
    duration: float


class ProviderHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_POST(self):
        server = cast(ProviderState, self.server)
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        server.requests.append(request)
        mode, gate = server.mode, server.allow
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        server.started.set()

        def emit(text, finish=None):
            value = {"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": finish}]}
            self.wfile.write(("data: " + json.dumps(value) + "\n\n").encode())
            self.wfile.flush()

        try:
            if mode == "long":
                started = time.monotonic()
                index = 0
                while time.monotonic() - started < server.duration:
                    emit(f"long tick {index}\n")
                    index += 1
                    time.sleep(0.25)
                emit("LONG_FINISHED", "stop")
            else:
                gate.wait(30)
                emit(f"{mode}_BODY", "stop")
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
        self.reject = False
        self.drop = False
        self.hold: threading.Event | None = None
        self.accepted = threading.Event()
        self.prompts = []
        self.attachments = 0
        self.expire = False


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
        route = self.path.split("?")[0]
        if route == "/attach":
            relay.attachments += 1
        if route == "/prompt":
            relay.prompts.append(json.loads(body)["request_id"])
        fault = None
        if route == "/prompt" and relay.reject:
            fault = {"code": "Busy", "source": "fixture", "message": "Admission rejected"}
        if route == "/tui/snapshot" and "attachment=" in self.path and relay.expire:
            relay.expire = False
            fault = {
                "code": "AttachmentExpired",
                "source": "fixture",
                "message": "Attachment expired",
            }
        if fault:
            data = json.dumps({"ok": False, "error": fault}).encode()
        else:
            connection = http.client.HTTPConnection(relay.address, timeout=180)
            try:
                connection.request(
                    self.command,
                    self.path,
                    body=body,
                    headers={"X-Eden-Token": self.headers["X-Eden-Token"]},
                )
                response = connection.getresponse()
                data = response.read()
            finally:
                connection.close()
            if route == "/prompt":
                relay.accepted.set()
                if relay.drop:
                    relay.drop = False
                    self.close_connection = True
                    return
                if relay.hold:
                    relay.hold.wait(15)
        try:
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass


def send(rpc, method, params):
    rpc.sequence += 1
    rpc.process.stdin.write(
        json.dumps({"jsonrpc": "2.0", "id": rpc.sequence, "method": method, "params": params})
        + "\n"
    )
    rpc.process.stdin.flush()
    return rpc.sequence


def observe(rpc, predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while True:
        match = next((event for event in rpc.events if predicate(event)), None)
        if match is not None:
            return match
        assert time.monotonic() < deadline, "expected correlated adapter event was not observed"
        rpc.events.append(rpc.messages.get(timeout=max(0.1, deadline - time.monotonic())))


def prompt(fixture, identity):
    return send(
        fixture.rpc,
        "session/prompt",
        {
            "sessionId": fixture.identity,
            "_meta": {"promptId": identity},
            "prompt": [{"type": "text", "text": identity}],
        },
    )


def accepted(fixture, identity):
    event = observe(
        fixture.rpc,
        lambda event: event.get("method") == "_x.ai/queue/changed"
        and event["params"].get("runningPromptId") == identity,
    )
    return event["params"]["_meta"]["edenRunId"]


def finish(fixture, request, status):
    event = observe(fixture.rpc, lambda event: event.get("id") == request)
    assert event.get("result", {}).get("stopReason") == status, event


def lifecycle(fixture):
    fixture.provider.RequestHandlerClass = ProviderHandler
    relay = Relay(fixture.endpoint["address"])
    threading.Thread(target=relay.serve_forever, daemon=True).start()
    relay_file = fixture.root / "relay.json"
    relay_file.write_text(
        json.dumps({**fixture.endpoint, "address": f"127.0.0.1:{relay.server_port}"})
    )
    fixture.rpc.close()
    fixture.rpc = Rpc(relay_file)
    fixture.rpc.call("session/load", {"sessionId": fixture.identity})
    try:
        relay.reject = True
        request = prompt(fixture, "rejected")
        reply = observe(fixture.rpc, lambda event: event.get("id") == request)
        assert "Admission rejected" in reply["error"]["message"]
        assert not any(
            event.get("params", {}).get("runningPromptId") == "rejected"
            for event in fixture.rpc.events
        )
        assert not fixture.provider.requests
        relay.reject = False
        fixture.checks["unaccepted_request_is_not_acknowledged"] = True

        fixture.provider.mode = "OLD_LATE"
        old_gate = threading.Event()
        fixture.provider.allow = old_gate
        relay.hold = threading.Event()
        request = prompt(fixture, "cancel-before-ack")
        assert relay.accepted.wait(10)
        start = time.monotonic()
        fixture.rpc.call(
            "session/cancel",
            {"sessionId": fixture.identity, "_meta": {"promptId": "cancel-before-ack"}},
        )
        assert time.monotonic() - start < 2, "cancellation blocked on acceptance response"
        relay.hold.set()
        relay.hold = None
        old_run = accepted(fixture, "cancel-before-ack")
        finish(fixture, request, "cancelled")
        fixture.checks["cancel_before_ack_retains_cancel_intent"] = True

        fixture.provider.started.clear()
        fixture.provider.mode = "NEW"
        fixture.provider.allow = threading.Event()
        relay.drop = True
        request = prompt(fixture, "new-prompt")
        run = accepted(fixture, "new-prompt")
        assert run != old_run
        assert fixture.provider.started.wait(10)
        fixture.rpc.call(
            "session/cancel",
            {"sessionId": fixture.identity, "_meta": {"promptId": "cancel-before-ack"}},
        )
        assert fixture.call("/tui/snapshot")["state"]["active_run"] == run
        old_gate.set()
        previous = relay.attachments
        relay.expire = True
        until = time.monotonic() + 10
        while relay.attachments <= previous:
            assert time.monotonic() < until, "expired attachment did not reconnect"
            time.sleep(0.05)
        fixture.provider.allow.set()
        finish(fixture, request, "end_turn")
        updates = [event for event in fixture.rpc.events if event.get("method") == "session/update"]
        body = [event for event in updates if "NEW_BODY" in json.dumps(event["params"]["update"])]
        assert body and all(
            event["params"]["_meta"].get("promptId") == "new-prompt" for event in body
        )
        assert all(event["params"]["_meta"].get("edenRunId") == run for event in body)
        assert any(event["params"]["_meta"].get("edenAttemptId") for event in body)
        assert not any("OLD_LATE_BODY" in json.dumps(event) for event in updates)
        assert len(set(relay.prompts)) == 3 and len(relay.prompts) == 3
        fixture.checks.update(
            {
                "lost_acceptance_reconciled_without_resubmission": True,
                "late_cancel_does_not_cancel_new_prompt": True,
                "late_provider_output_does_not_cross_attempts": True,
                "projection_carries_prompt_run_attempt": True,
                "expired_attachment_reconnects_same_run": True,
            }
        )
        fixture.provider.started.clear()
        fixture.provider.mode = "REATTACH"
        fixture.provider.allow = threading.Event()
        request = prompt(fixture, "reattach-origin")
        run = accepted(fixture, "reattach-origin")
        assert fixture.provider.started.wait(10)
        viewer = Rpc(relay_file)
        try:
            loaded = viewer.call("session/load", {"sessionId": fixture.identity})
            running = loaded.get("_meta", {}).get("x.ai/runningPromptId")
            assert running, "live attachment did not report its running prompt"
            viewer.call(
                "session/cancel", {"sessionId": fixture.identity, "_meta": {"promptId": running}}
            )
            finish(fixture, request, "cancelled")
            ended = observe(
                viewer, lambda event: event.get("method") == "_x.ai/session/prompt_complete"
            )
            assert ended["params"]["promptId"] == running
            assert ended["params"]["stopReason"] == "cancelled"
            fixture.checks["live_reattach_adopts_and_cancels_the_existing_run"] = True
        finally:
            fixture.provider.allow.set()
            viewer.close()
        fixture.provider.started.clear()
        fixture.provider.mode = "PTY_REATTACH"
        fixture.provider.allow = threading.Event()
        request = prompt(fixture, "pty-reattach-origin")
        accepted(fixture, "pty-reattach-origin")
        assert fixture.provider.started.wait(10)
        fixture.terminal_start()
        fixture.terminal.wait("Session opened")
        fixture.terminal.send(b"ATTACHED_DRAFT")
        fixture.terminal.wait("ATTACHED_DRAFT")
        fixture.terminal.send(b"\x03")
        finish(fixture, request, "cancelled")
        fixture.terminal.wait("ATTACHED_DRAFT")
        fixture.capture("live-reattach-cancel")
        fixture.provider.allow.set()
        fixture.checks["pty_reattach_ctrl_c_cancels_owner_and_preserves_draft"] = True
        fixture.stop_terminal()
        fixture.terminal = None

        fixture.provider.started.clear()
        fixture.provider.mode = "REATTACH_DONE"
        fixture.provider.allow = threading.Event()
        request = prompt(fixture, "reattach-completion")
        accepted(fixture, "reattach-completion")
        viewer = Rpc(relay_file)
        try:
            loaded = viewer.call("session/load", {"sessionId": fixture.identity})
            running = loaded["_meta"]["x.ai/runningPromptId"]
            fixture.provider.allow.set()
            finish(fixture, request, "end_turn")
            ended = observe(
                viewer, lambda event: event.get("method") == "_x.ai/session/prompt_complete"
            )
            assert ended["params"]["promptId"] == running
            assert ended["params"]["stopReason"] == "end_turn"
            fixture.checks["reattached_view_receives_natural_terminal"] = True
        finally:
            fixture.provider.allow.set()
            viewer.close()
    finally:
        if relay.hold:
            relay.hold.set()
        fixture.provider.allow.set()
        relay.shutdown()
        relay.server_close()


def long_stream(fixture, seconds):
    fixture.provider.RequestHandlerClass = ProviderHandler
    fixture.provider.mode = "long"
    fixture.provider.duration = seconds
    fixture.terminal_start()
    start = time.monotonic()
    fixture.terminal.command("Stream past the original acknowledgement deadline.")
    assert fixture.provider.started.wait(10)
    run = fixture.call("/tui/snapshot")["state"]["active_run"]
    assert run is not None
    while "LONG_FINISHED" not in fixture.terminal.display:
        assert time.monotonic() - start < seconds + 20, fixture.terminal.display
        fixture.terminal.read(0.25)
    result = fixture.call("/terminal", {"run_id": run})
    assert result["outcome"]["status"] == "completed", result
    elapsed = time.monotonic() - start
    assert elapsed >= seconds
    fixture.capture("long-stream-complete")
    fixture.checks["long_stream_seconds"] = round(elapsed, 3)
    fixture.checks["original_120_second_deadline_no_longer_cancels_accepted_run"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/g1-native-host")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--long", action="store_true")
    parser.add_argument("--seconds", type=int, default=130)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture:
        if args.long:
            long_stream(fixture, args.seconds)
        else:
            lifecycle(fixture)


if __name__ == "__main__":
    main()
