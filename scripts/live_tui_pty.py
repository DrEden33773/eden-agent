"""Isolated real-PTY regression for presentation input and recovery (POSIX)."""

import copy
import fcntl
import http.server
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from pathlib import Path
from typing import cast


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
            "state": {"active_run": 1 if self.active else None, "read_only": self.read_only},
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
                    "code": "InvalidInput",
                    "message": "unknown attachment",
                }
            )
        else:
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
                self.respond(error={"source": "live", "code": "InputFailure", "message": "lost"})
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


class Terminal:
    def __init__(self, binary, endpoint):
        self.master, self.slave = pty.openpty()
        self.cells = [[" "] * 60 for _ in range(12)]
        self.row = self.column = 0
        self.before = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 0, 0))
        self.process = subprocess.Popen(
            [binary, "live-tui", "--endpoint", str(endpoint)],
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            env={**os.environ, "TERM": "xterm-256color"},
        )
        self.read(0.5)

    def read(self, seconds=0.35):
        result = bytearray()
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            ready, _, _ = select.select([self.master], [], [], 0.03)
            if ready:
                result.extend(os.read(self.master, 65536))
        self.paint(bytes(result).decode("utf-8", errors="replace"))
        return bytes(result)

    def paint(self, data):
        # Only the CSI cursor/erase operations emitted by Ratatui in this fixture.
        for token in re.findall(r"\x1b\[[0-?]*[ -/]*[@-~]|[^\x1b]+", data):
            if token.startswith("\x1b["):
                arguments, command = token[2:-1], token[-1]
                if command == "H":
                    parts = arguments.split(";")
                    self.row = int(parts[0] or "1") - 1
                    self.column = int(parts[1] or "1") - 1 if len(parts) > 1 else 0
                elif command == "J" and arguments == "2":
                    self.cells = [[" "] * 60 for _ in range(12)]
                elif command == "K" and 0 <= self.row < 12:
                    self.cells[self.row][self.column :] = [" "] * (60 - self.column)
            else:
                for character in token:
                    if character == "\r":
                        self.column = 0
                    elif character == "\n":
                        self.row += 1
                    elif character >= " ":
                        if 0 <= self.row < 12 and 0 <= self.column < 60:
                            self.cells[self.row][self.column] = character
                        self.column += 1

    @property
    def display(self):
        return "\n".join("".join(row) for row in self.cells)

    def send(self, data):
        os.write(self.master, data)
        return self.read()

    def close(self):
        self.send(b"\x1b")
        self.process.wait(timeout=3)
        assert termios.tcgetattr(self.slave) == self.before, "terminal modes not restored"
        os.close(self.master)
        os.close(self.slave)


def stalled_attach_recovery(binary):
    server = Fixture()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="eden-tui-reattach-") as directory:
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
            terminal = Terminal(binary, endpoint)
            try:
                terminal.send(b"\tX")
                server.stall_attach = True
                server.expired = True
                assert server.attach_started.wait(3), "recovery attach never started"
                terminal.read(0.5)
                assert "Disconnected" in terminal.display, terminal.display
                wait_for(
                    lambda: server.attachment == 2, "stalled attach blocked recovery", seconds=9
                )
                terminal.read(0.5)
                assert "Connected" in terminal.display, terminal.display
                terminal.send(b"\r")
                wait_for(lambda: server.actions, "recovered form never submitted")
                assert server.actions[-1]["values"]["text"] == "baseX"
                assert not server.attach_release.is_set(), (
                    "original attach released before recovery"
                )
                print("stalled reattach timeout, automatic recovery and draft retention: passed")
            finally:
                terminal.close()
    finally:
        server.attach_release.set()
        server.shutdown()
        server.server_close()


def main():
    binary = str(Path(sys.argv[1]).resolve())
    stalled_attach_recovery(binary)
    if sys.argv[2:] == ["--attach-recovery-only"]:
        return
    server = Fixture()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="eden-tui-pty-") as directory:
        endpoint = Path(directory) / "endpoint.json"
        endpoint.write_text(
            json.dumps(
                {"address": f"127.0.0.1:{server.server_port}", "token": "test", "session_id": 123}
            )
        )
        terminal = Terminal(binary, endpoint)
        try:
            terminal.send(b"\tx\x7fZ")
            terminal.send(b"\x1b[F")
            assert "TAIL-REACHED" in terminal.display, terminal.display
            server.sequence += 1
            server.fields[0]["initial"] = "replacement"
            terminal.read()
            assert "TAIL-REACHED" in terminal.display, "live revision reset scroll"
            assert "baseZ" in terminal.display, "focused value hidden on narrow screen"
            terminal.send(b"\t \t\x1b[C\t \r")
            wait_for(lambda: server.actions, "form never submitted")
            assert server.actions[-1]["values"] == {
                "text": "baseZ",
                "flag": False,
                "choice": "c",
                "multi": [],
            }, server.actions[-1]
            print("prefilled edits and revision draft retention: passed")
            terminal.send(b"\tABC")
            time.sleep(3.2)
            terminal.read()
            count = len(server.activities)
            terminal.send(b"\x7f")
            assert any(
                item["active"] and item["target"] == {"kind": "composer"}
                for item in server.activities[count:]
            ), "backspace activity missing"
            print("backspace activity after TTL: passed")
            server.fail = True
            terminal.read(0.9)
            assert "Disconnected" in terminal.display, terminal.display
            server.expired = True
            server.fail = False
            server.sequence += 1
            wait_for(lambda: server.attachment == 2, "attachment not renewed")
            terminal.read(0.6)
            terminal.send(b"\t\r")
            wait_for(lambda: len(server.actions) == 2, "recovered form not usable")
            assert server.actions[-1]["values"]["text"] == "baseZ"
            print("snapshot recovery, reattach and draft retention: passed")
            server.mode = "uncertain"
            terminal.send(b"\r")
            wait_for(lambda: len(server.actions) == 3, "uncertain attempt absent")
            terminal.read()
            server.mode = "ok"
            terminal.send(b"\x12")
            wait_for(lambda: len(server.actions) == 4, "retry absent")
            assert server.actions[-1]["request_id"] == server.actions[-2]["request_id"]
            print("uncertain retry retains request identity: passed")
            server.mode = "slow"
            terminal.send(b"\r")
            wait_for(lambda: len(server.actions) == 5, "slow action absent")
            terminal.send(b"\x18")
            assert server.cancelled.wait(2), "cancel blocked behind action response"
            terminal.close()
            terminal = None
            assert not server.release.is_set(), "slow barrier unexpectedly released"
            print("slow action cancel, detach and terminal restoration: passed")
        finally:
            server.release.set()
            if terminal:
                terminal.close()
        server.read_only = True
        server.active = False
        terminal = Terminal(binary, endpoint)
        try:
            terminal.send(b"\x1b[F")
            assert "TAIL-REACHED" in terminal.display, terminal.display
            terminal.send(b"\x1b[1;3C" * 12)
            assert "RIGHT-EDGE" in terminal.display, terminal.display
            server.sequence += 1
            terminal.read()
            assert "RIGHT-EDGE" in terminal.display, "revision reset scroll"
            print("read-only narrow viewport vertical/horizontal scroll retention: passed")
        finally:
            terminal.close()
        server.read_only = False
        server.active = True
        server.mode = "ok"
        for slot in ("header", "footer", "overlay", "composer"):
            server.active = True
            server.hide_view = False
            server.remove_on_cancel = True
            server.slot = slot
            server.fields[0]["initial"] = "base"
            server.cancelled.clear()
            terminal = Terminal(binary, endpoint)
            try:
                count = len(server.actions)
                terminal.send(b"kept\tX")
                assert "text:" in terminal.display, terminal.display
                server.sequence += 1
                terminal.read()
                terminal.send(b"\r")
                wait_for(
                    lambda count=count: len(server.actions) > count, f"{slot} action unreachable"
                )
                assert server.actions[-1]["values"]["text"] == "baseX"
                terminal.send(b"\x18")
                assert server.cancelled.wait(2), f"{slot} cancel unreachable"
                terminal.read(0.5)
                assert "Composer: kept" in terminal.display, terminal.display
                activity_count = len(server.activities)
                terminal.send(b"Y")
                assert "Composer: keptY" in terminal.display, terminal.display
                assert any(
                    item["active"] and item["target"] == {"kind": "composer"}
                    for item in server.activities[activity_count:]
                ), f"{slot} did not restore editable composer focus"
            finally:
                terminal.close()
            print(
                f"{slot} focus, revision draft preservation, cancel and composer focus restoration: passed"
            )
        server.hide_view = False
        server.active = True
        server.unknown = True
        for version in (1, 999):
            server.version = version
            terminal = Terminal(binary, endpoint)
            try:
                terminal.send(b"\x1b[F")
                expected = "FUTURE-NODE-FALLBACK" if version == 1 else "FUTURE-VIEW-FALLBACK"
                assert expected in terminal.display, terminal.display
                if version == 1:
                    assert "KNOWN-CHILD" in terminal.display, terminal.display
                count = len(server.actions)
                terminal.send(b"\t\r")
                assert len(server.actions) == count, "fallback exposed action"
            finally:
                terminal.close()
            print(f"vocabulary {version} safe fallback: passed")
    server.shutdown()


if __name__ == "__main__":
    main()
