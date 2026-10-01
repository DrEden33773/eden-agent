"""Real POSIX terminal driver; Windows callers explicitly report PTY as skipped."""

import codecs
import os
import re
import select
import signal
import struct
import subprocess
import tempfile
import time
import unicodedata
from pathlib import Path


class Terminal:
    """Keep the slave open to verify the exact termios state after normal exit."""

    def __init__(self, binary, endpoint, *, env=None, width=110, height=32, command=None):
        if os.name == "nt":
            raise RuntimeError("POSIX PTY unavailable; run native terminal checks separately")
        import pty
        import termios

        self.state = tempfile.TemporaryDirectory(prefix="eden-tui-state-")
        self.master, self.slave = pty.openpty()
        self.before = termios.tcgetattr(self.slave)
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.row = self.column = 0
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""
        self.output = bytearray()
        self.closed = False
        self.resize(width, height)
        environment = {
            **os.environ,
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "EDEN_TUI_STATE_DIR": self.state.name,
        }
        environment.pop("NO_COLOR", None)
        environment.update(env or {})
        self.process = subprocess.Popen(
            command or [str(binary), "tui", "--frontend", "native", "--endpoint", str(endpoint)],
            cwd=Path(endpoint).parent,
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            env=environment,
        )

    def resize(self, width, height):
        import fcntl
        import termios

        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        if hasattr(self, "process"):
            os.kill(self.process.pid, signal.SIGWINCH)

    def read(self, seconds=0.15):
        result = bytearray()
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            ready, _, _ = select.select(
                [self.master], [], [], min(0.03, max(0, until - time.monotonic()))
            )
            if ready:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                result.extend(chunk)
        self.output.extend(result)
        self.paint(self.decoder.decode(bytes(result)))
        return bytes(result)

    def drain(self):
        """Paint all bytes already acknowledged by the terminal writer."""
        result = bytearray()
        while select.select([self.master], [], [], 0)[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                break
            if not chunk:
                break
            result.extend(chunk)
        self.output.extend(result)
        self.paint(self.decoder.decode(bytes(result)))
        return bytes(result)

    def paint(self, data):
        data = self.pending + data
        self.pending = ""
        while data:
            if data.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-?]*[ -/]*)([@-~])", data)
                if not match:
                    self.pending = data
                    break
                args, command = match.groups()
                data = data[match.end() :]
                parts = (
                    [int(v or 0) for v in args.split(";")] if not args or args[0].isdigit() else []
                )
                value = (parts[0] or 1) if parts else 1
                if command == "n" and args == "6":
                    os.write(
                        self.master,
                        f"\x1b[{min(self.row + 1, self.height)};{min(self.column + 1, self.width)}R".encode(),
                    )
                elif command in ("H", "f"):
                    self.row = value - 1
                    self.column = (parts[1] or 1) - 1 if len(parts) > 1 else 0
                elif command == "G":
                    self.column = value - 1
                elif command == "A":
                    self.row = max(0, self.row - value)
                elif command == "B":
                    self.row += value
                elif command == "C":
                    self.column += value
                elif command == "D":
                    self.column = max(0, self.column - value)
                elif command == "J" and args == "2":
                    self.cells = [[" "] * self.width for _ in range(self.height)]
                elif command == "K" and 0 <= self.row < self.height:
                    start = 0 if args == "2" else min(self.column, self.width)
                    self.cells[self.row][start:] = [" "] * (self.width - start)
            elif data.startswith("\x1b"):
                if len(data) == 1:
                    self.pending = data
                    break
                data = data[2:]
            else:
                character, data = data[0], data[1:]
                if character == "\r":
                    self.column = 0
                elif character == "\n":
                    self.row += 1
                elif character >= " ":
                    width = (
                        0
                        if unicodedata.combining(character)
                        else 2
                        if unicodedata.east_asian_width(character) in ("W", "F")
                        else 1
                    )
                    if 0 <= self.row < self.height and 0 <= self.column < self.width:
                        self.cells[self.row][self.column] = character
                        if width == 2 and self.column + 1 < self.width:
                            self.cells[self.row][self.column + 1] = ""
                    self.column += width

    @property
    def display(self):
        return "\n".join("".join(row) for row in self.cells)

    def wait(self, text, seconds=8):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            self.read(0.1)
            if text in self.display:
                return
            if self.process.poll() is not None:
                break
        raise AssertionError(
            f"Terminal did not show {text!r} (exit={self.process.poll()}):\n{self.display}\n{bytes(self.output[-500:])!r}"
        )

    def send(self, data):
        os.write(self.master, data)
        return self.read()

    def command(self, text):
        self.send(text.encode())
        self.send(b"\r")

    def close(self, *, expected_code: int | None = 0, screen=True):
        import termios

        if self.closed:
            return
        try:
            if self.process.poll() is None:
                # Ensure Esc closes a local dialog instead of cancelling a foreground run.
                self.send(b"\x1bOP")  # F1 opens Help when no dialog is present.
                self.send(b"\x1b")
                self.send(b"\x03\x04")
                until = time.monotonic() + 5
                while self.process.poll() is None and time.monotonic() < until:
                    self.read(0.1)
            code = self.process.wait(timeout=1)
            assert code == expected_code if expected_code is not None else code != 0, bytes(
                self.output[-1000:]
            )
            self.read()
            assert termios.tcgetattr(self.slave) == self.before, "terminal modes not restored"
            if screen:
                assert b"\x1b[?1049l" in self.output, "alternate screen not restored"
                assert b"\x1b[?2004l" in self.output, "bracketed paste not disabled"
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            os.close(self.master)
            os.close(self.slave)
            self.state.cleanup()
            self.closed = True
