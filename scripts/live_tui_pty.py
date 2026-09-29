"""Isolated real-PTY regression for presentation input and recovery (POSIX)."""

import copy
import http.server
import json
import os
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import cast

from tui_pty import Terminal


class Fixture(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.actions = []
        self.activities = []
        self.cancelled = threading.Event()
        self.release = threading.Event()
        self.fail = False
        self.expired = False
        self.attachment = 0
        self.stall_attach = False
        self.attach_started = threading.Event()
        self.attach_release = threading.Event()
        self.recovery_snapshot_delay = 0.0
        self.sequence = 1
        self.read_only = False
        self.active = True
        self.hide_view = False
        self.remove_on_cancel = False
        self.mode = "ok"
        self.slot = "panel"
        self.version = 1
        self.unknown = False
        self.fields = [
            {
                "id": name,
                "label": name,
                "kind": kind,
                "required": False,
                "initial": initial,
                "options": options,
            }
            for name, kind, initial, options in [
                ("text", "text", "base", []),
                ("flag", "boolean", True, []),
                ("choice", "choice", "b", ["a", "b", "c"]),
                ("multi", "multi_choice", ["a"], ["a", "b", "c"]),
            ]
        ]

    def snapshot(self):
        return {
            "state": {
                "session_id": 123,
                "closed": False,
                "active_run": 1 if self.active else None,
                "read_only": self.read_only,
            },
            "history": [],
            "events": [],
            "presentation": {
                "version": self.version,
                "session_id": "123",
                "sequence": self.sequence,
                "activity": [],
                "pending_interactions": [],
                "views": []
                if self.hide_view
                else [
                    {
                        "owner": "fixture",
                        "run_id": 1,
                        "revision": self.sequence,
                        "active": self.active,
                        "id": "view",
                        "slot": self.slot,
                        "title": "Fixture",
                        "fallback": "FUTURE-VIEW-FALLBACK",
                        "source": None,
                        "platforms": [],
                        "nodes": (
                            [
                                {
                                    "kind": "future-node",
                                    "id": "future",
                                    "fallback": "FUTURE-NODE-FALLBACK",
                                    "children": [
                                        {"kind": "text", "id": "known", "text": "KNOWN-CHILD"}
                                    ],
                                }
                            ]
                            if self.unknown
                            else [
                                {
                                    "kind": "form",
                                    "id": "form",
                                    "action": "submit",
                                    "fields": copy.deepcopy(self.fields),
                                },
                                {
                                    "kind": "table",
                                    "id": "table",
                                    "columns": ["rows"],
                                    "rows": [[f"row-{i:02d}"] for i in range(30)],
                                },
                                {
                                    "kind": "diff",
                                    "id": "diff",
                                    "before": "before",
                                    "after": "TAIL-REACHED " + "x" * 80 + "RIGHT-EDGE",
                                },
                            ]
                        ),
                    }
                ],
            },
        }


class Handler(http.server.BaseHTTPRequestHandler):
    @property
    def fixture(self):
        return cast(Fixture, self.server)

    def log_message(self, format: str, *args: object) -> None:
        pass

    def respond(self, result=None, error=None):
        data = json.dumps({"ok": error is None, "result": result, "error": error}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        time.sleep(0.05)
        if self.fixture.fail:
            self.respond(error={"source": "live", "code": "Unavailable", "message": "offline"})
        elif self.fixture.expired:
            self.respond(
                error={
                    "source": "presentation",
                    "code": "AttachmentExpired",
                    "message": "attachment lease ended",
                }
            )
        else:
            if self.fixture.attachment >= 2 and self.fixture.recovery_snapshot_delay:
                delay = self.fixture.recovery_snapshot_delay
                self.fixture.recovery_snapshot_delay = 0.0
                time.sleep(delay)
            self.respond(self.fixture.snapshot())

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/attach":
            if self.fixture.stall_attach:
                self.fixture.stall_attach = False
                self.fixture.attach_started.set()
                self.fixture.attach_release.wait()
                self.respond({"attachment": 999})
                return
            self.fixture.attachment += 1
            self.fixture.expired = False
            self.respond({"attachment": self.fixture.attachment})
        elif self.path == "/action":
            self.fixture.actions.append(body)
            if self.fixture.mode == "slow":
                self.fixture.release.wait(15)
            if self.fixture.mode == "uncertain":
                self.close_connection = True  # Accepted action, but its HTTP response was lost.
            else:
                self.respond({"accepted": True})
        elif self.path == "/cancel":
            if self.fixture.remove_on_cancel:
                self.fixture.hide_view = True
                self.fixture.active = False
                self.fixture.sequence += 1
            self.fixture.cancelled.set()
            self.respond({})
        elif self.path == "/activity":
            self.fixture.activities.append(body)
            self.respond({})
        else:
            self.respond({})


def wait_for(predicate, message, seconds=5):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if predicate():
            return
        time.sleep(0.03)
    raise AssertionError(message)


def open_form(terminal):
    terminal.command("/live")
    terminal.wait("Plugin views")
    terminal.send(b"\r")
    terminal.wait("Ctrl+S Apply")


def exercise(binary, endpoint, server):
    terminal = Terminal(binary, endpoint)
    try:
        terminal.wait("Connected")
        terminal.send(b"\x1b[200~/quit\n!echo never\n" + "中文🙂".encode() + b"\x1b[201~")
        assert terminal.process.poll() is None, "paste executed /quit"
        assert not server.actions, "paste submitted an action"
        terminal.send(b"\x03")
        terminal.send(b"ABC")
        terminal.read(3.2)
        activity_count = len(server.activities)
        terminal.send(b"\x7f")
        wait_for(
            lambda: any(
                item["active"] and item["target"] == {"kind": "composer"}
                for item in server.activities[activity_count:]
            ),
            "backspace activity after TTL missing",
        )
        terminal.send(b"\x03")
        print("backspace activity after TTL: passed")
        open_form(terminal)
        terminal.send(b"x\x7fZ")
        server.sequence += 1
        server.fields[0]["initial"] = "replacement"
        terminal.read()
        terminal.send(b"\t \t\x1b[C\t ")
        terminal.send(b"\r\t")
        terminal.send(b"\x13")
        wait_for(lambda: server.actions, "form never submitted")
        assert server.actions[-1]["values"] == {
            "text": "baseZ",
            "flag": False,
            "choice": "c",
            "multi": [],
        }, server.actions[-1]
        print("bracketed paste, typed fields and revision draft retention: passed")
        server.fail = True
        terminal.wait("Disconnected")
        server.fail = False
        terminal.wait("Connected")
        terminal.send(b"\x13")
        wait_for(lambda: len(server.actions) == 2, "recovered form not usable")
        assert server.actions[-1]["values"]["text"] == "baseZ"
        server.mode = "uncertain"
        terminal.send(b"\x13")
        wait_for(lambda: len(server.actions) == 3, "uncertain attempt absent")
        terminal.read()
        server.mode = "ok"
        terminal.send(b"\x1b")
        terminal.send(b"\x12")
        wait_for(lambda: len(server.actions) == 4, "retry absent")
        assert server.actions[-1]["request_id"] == server.actions[-2]["request_id"]
        print("connection recovery and uncertain action request identity: passed")
        open_form(terminal)
        server.mode = "slow"
        terminal.send(b"\x13")
        wait_for(lambda: len(server.actions) == 5, "slow action absent")
        terminal.send(b"\x1b")
        assert not server.cancelled.is_set(), "closing a form cancelled execution"
        terminal.send(b"\x1b")
        assert server.cancelled.wait(2), "cancel blocked behind action response"
        terminal.close()
        assert not server.release.is_set(), "slow barrier unexpectedly released"
        print("local Esc, foreground cancel, detach and terminal restoration: passed")
    finally:
        server.release.set()
        terminal.close()
    server.mode = "ok"
    for slot in ("header", "footer", "overlay", "composer", "panel"):
        server.slot = slot
        server.fields[0]["initial"] = "base"
        server.cancelled.clear()
        terminal = Terminal(binary, endpoint)
        try:
            terminal.wait("Connected")
            open_form(terminal)
            count = len(server.actions)
            terminal.send(b"X")
            terminal.send(b"\x1b")
            open_form(terminal)
            terminal.send(b"\x13")
            wait_for(lambda count=count: len(server.actions) > count, f"{slot} action unreachable")
            assert server.actions[-1]["values"]["text"] == "baseX"
            terminal.send(b"\x1b")
            terminal.send(b"kept")
            terminal.wait("kept")
        finally:
            terminal.close()
        print(f"{slot} form reachability, retained draft and restored composer: passed")
    server.read_only = True
    server.active = False
    server.slot = "panel"
    terminal = Terminal(binary, endpoint)
    try:
        terminal.wait("Read only")
        count = len(server.actions)
        terminal.command("must not submit")
        terminal.send(b"\x03")
        terminal.command("/live")
        terminal.send(b"\r\x13")
        assert len(server.actions) == count, "read-only session exposed an action"
        terminal.send(b"\x1b")
        terminal.send(b"\x03")
        terminal.resize(60, 16)
        terminal.wait("Connected")
        terminal.send(b"\x1b[5~")
        before = terminal.display.splitlines()[1]
        server.sequence += 1
        terminal.read()
        assert terminal.display.splitlines()[1] == before, "revision reset reading anchor"
        terminal.send(b"\x1b[19~")  # F8 returns to latest.
        terminal.wait("RIGHT-EDGE")
    finally:
        terminal.close()
    print("read-only action exclusion and narrow resize: passed")
    server.read_only = False
    server.active = True
    server.unknown = True
    for version in (1, 999):
        server.version = version
        terminal = Terminal(binary, endpoint)
        try:
            terminal.wait("Connected")
            expected = "FUTURE-NODE-FALLBACK" if version == 1 else "FUTURE-VIEW-FALLBACK"
            terminal.wait(expected)
            if version == 1:
                terminal.wait("KNOWN-CHILD")
            count = len(server.actions)
            terminal.command("/live")
            terminal.send(b"\r\x13")
            assert len(server.actions) == count, "fallback exposed action"
        finally:
            terminal.close()
        print(f"vocabulary {version} safe fallback: passed")


def stalled_attach_recovery(binary, endpoint, server):
    terminal = Terminal(binary, endpoint)
    try:
        terminal.wait("Connected")
        open_form(terminal)
        terminal.send(b"X")
        before = server.attachment
        server.stall_attach = True
        server.recovery_snapshot_delay = 0.8
        server.expired = True
        terminal.wait("Disconnected")
        assert server.attach_started.wait(3), "recovery attach never started"
        wait_for(lambda: server.attachment > before, "stalled attach blocked recovery", seconds=15)
        terminal.wait("Connected")
        terminal.send(b"\x13")
        wait_for(lambda: server.actions, "recovered form never submitted")
        assert server.actions[-1]["values"]["text"] == "baseX"
        assert not server.attach_release.is_set(), "original attach released before recovery"
        print("stalled reattach timeout, automatic recovery and draft retention: passed")
    finally:
        server.attach_release.set()
        terminal.close()


def main():
    if os.name == "nt":
        print("SKIP: POSIX PTY unavailable on Windows; native terminal evidence required")
        return
    binary = str(Path(sys.argv[1]).resolve())
    server = Fixture()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="eden-tui-pty-") as directory:
            endpoint = Path(directory) / "endpoint.json"
            endpoint.write_text(
                json.dumps(
                    {
                        "address": f"127.0.0.1:{server.server_port}",
                        "token": "test",
                        "session_id": 123,
                    }
                )
            )
            if sys.argv[2:] == ["--attach-recovery-only"]:
                stalled_attach_recovery(binary, endpoint, server)
            else:
                stalled_attach_recovery(binary, endpoint, server)
                server.actions.clear()
                exercise(binary, endpoint, server)
    finally:
        server.attach_release.set()
        server.release.set()
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
