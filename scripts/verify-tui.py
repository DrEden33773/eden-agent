#!/usr/bin/env python3
"""Installed official terminal, saved-history and independent UI-author acceptance.

The default uses the shared frozen installation and SDK-author preparation. Explicit
--binary/--composition/--author paths exercise existing bytes without reinstalling.
No model/provider request or system clipboard read is made.
"""

import argparse
import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from install import ROOT
from tui_pty import Terminal
from verification import author_artifact, installed, prepare

spec = importlib.util.spec_from_file_location(
    "presentation_acceptance", ROOT / "scripts/verify-presentation.py"
)
assert spec and spec.loader
presentation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(presentation)
call = presentation.call


def wait_for(endpoint, predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        frame = call(endpoint, "/tui/snapshot")
        if predicate(frame):
            return frame
        time.sleep(0.03)
    raise AssertionError("TUI snapshot did not reach expected history")


def ready(process, endpoint):
    deadline = time.monotonic() + 15
    while not endpoint.exists() and time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(process.communicate())
        time.sleep(0.03)
    assert endpoint.exists(), "host did not publish endpoint"
    return json.loads(endpoint.read_text())


def stop(process, endpoint):
    if process.poll() is None:
        try:
            call(endpoint, "/shutdown", {})
            assert process.wait(timeout=15) == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def write_receipt(result: dict, destination: Path | None) -> None:
    """Default installed runs leave the same JSON receipt consumed by verify-all."""
    if destination is not None:
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--composition", type=Path)
    parser.add_argument("--author", type=Path)
    parser.add_argument("--editor", type=Path)
    args = parser.parse_args()
    receipt = None if args.binary else ROOT / "artifacts/tui-verification.json"
    if receipt is not None:
        receipt.unlink(missing_ok=True)
    if os.name == "nt":
        write_receipt(
            {
                "status": "skipped",
                "platform": sys.platform,
                "pty": "skipped",
                "reason": "POSIX PTY unavailable; native Windows terminal evidence required",
            },
            receipt,
        )
        return
    if args.binary:
        if not args.author or not args.composition:
            parser.error("--binary requires --composition and --author")
        binary, composition, author = (
            args.binary.resolve(),
            args.composition.resolve(),
            args.author.resolve(),
        )
    else:
        prepare()
        destination = installed(ROOT / "artifacts/install-tui")
        binary, composition, author = (
            destination / "bin/eden",
            destination / "composition.json",
            author_artifact("tui-editor"),
        )
    env = {"EDEN_TUI_EDITOR": str(args.editor.resolve())} if args.editor else {}
    with tempfile.TemporaryDirectory(prefix="eden-official-tui-") as directory:
        scratch = Path(directory)
        endpoint_file, history = scratch / "endpoint.json", scratch / "history.jsonl"
        editor_command = scratch / "external-editor"
        editor_command.write_text(
            f"#!{sys.executable}\nimport pathlib, sys, termios\nassert termios.tcgetattr(0)[3] & termios.ICANON\npathlib.Path(sys.argv[1]).write_text('external-editor-result')\n"
        )
        editor_command.chmod(0o700)
        env["VISUAL"] = str(editor_command)
        host = subprocess.Popen(
            [
                str(binary),
                "--composition",
                str(composition),
                "--cwd",
                str(scratch),
                "--global-dir",
                str(scratch / "global"),
                "--session",
                str(history),
                "--offline-startup",
                "--no-trust-project",
                "live",
                "--endpoint",
                str(endpoint_file),
            ],
            cwd=scratch,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        endpoint = ready(host, endpoint_file)
        try:
            terminal = Terminal(binary, endpoint_file, env=env)
            try:
                terminal.wait("Connected")
                terminal.send(b"\x07")
                terminal.wait("external-editor-result")
                terminal.send(b"\x03")
                editor_command.write_text(f"#!{sys.executable}\nraise SystemExit(3)\n")
                terminal.send(b"retained-external-draft\x07")
                terminal.wait("original draft retained")
                assert "retained-external-draft" in terminal.display
                terminal.send(b"\x1b\x1a")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    pid, status = os.waitpid(terminal.process.pid, os.WNOHANG | os.WUNTRACED)
                    if pid and os.WIFSTOPPED(status):
                        break
                    terminal.read(0.05)
                else:
                    raise AssertionError("terminal did not suspend")
                import termios

                assert termios.tcgetattr(terminal.slave) == terminal.before, (
                    "suspend retained raw mode"
                )
                os.kill(terminal.process.pid, signal.SIGCONT)
                terminal.read()
                terminal.send(b"\x03")
                terminal.send(b"\x1b[200~/quit\n!printf must-not-run\n\x1b[201~")
                assert call(endpoint, "/snapshot")["state"]["active_run"] is None
                assert not call(endpoint, "/snapshot")["state"]["shell_runs"]
                terminal.send(b"\x03")
                terminal.command("/shell printf SAVED_TUI_HISTORY")
                wait_for(
                    endpoint,
                    lambda frame: any(
                        record["kind"] == "user_shell" for record in frame["history"]
                    ),
                )
                terminal.send(b"\x0f")
                terminal.wait("SAVED_TUI_HISTORY")
                terminal.resize(60, 16)
                terminal.wait("Connected")
                terminal.resize(110, 32)
                terminal.wait("F1 help")
            finally:
                terminal.close()
            assert host.poll() is None, "detach stopped the shared host"
            terminal = Terminal(binary, endpoint_file, env={**env, "EDEN_TUI_EDITOR": str(author)})
            try:
                terminal.wait("Connected")
                terminal.send(b"author-input-marker")
                assert (
                    "author-input-marker" not in terminal.display and "› A" in terminal.display
                ), terminal.display
                terminal.send(b"\x03")
                terminal.command("/shell printf AUTHOR_EDITOR_INPUT")
                wait_for(
                    endpoint,
                    lambda frame: any(
                        record["kind"] == "user_shell"
                        and "AUTHOR_EDITOR_INPUT" in json.dumps(record)
                        for record in frame["history"]
                    ),
                )
                deadline = time.monotonic() + 8
                while terminal.display.count("✓ bash · user command") < 2:
                    terminal.read(0.1)
                    if time.monotonic() >= deadline:
                        raise AssertionError(
                            f"Second durable shell card did not render:\n{terminal.display}"
                        )
                terminal.send(b"\x1b[17~\x1b[B\x0f\x1b[17~")
                terminal.wait("AUTHOR_EDITOR_INPUT")
            finally:
                terminal.close()
            terminal = Terminal(
                binary,
                endpoint_file,
                env={**env, "EDEN_TUI_RENDERER": str(author)},
            )
            try:
                terminal.wait("AUTHOR RENDERER")
            finally:
                terminal.close()
            terminal = Terminal(binary, endpoint_file, env={**env, "EDEN_TUI_THEME": str(author)})
            try:
                terminal.wait("Connected")
                assert b"38;2;18;171;52" in terminal.output, "independent theme accent not emitted"
            finally:
                terminal.close()
            terminal = Terminal(
                binary, endpoint_file, env={**env, "EDEN_TUI_FRONTEND": str(author)}
            )
            try:
                terminal.wait("AUTHOR FRONTEND session")
            finally:
                terminal.close(screen=False)
        finally:
            stop(host, endpoint)
        reader_endpoint = scratch / "read.json"
        reader = subprocess.Popen(
            [
                str(binary),
                "--offline-startup",
                "read",
                str(history),
                "--endpoint",
                str(reader_endpoint),
            ],
            cwd=scratch,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        endpoint = ready(reader, reader_endpoint)
        try:
            terminal = Terminal(binary, reader_endpoint, env=env)
            try:
                terminal.wait("Read only")
                terminal.send(b"\x1b[17~\x1b[B\x0f\x1b[17~")
                terminal.wait("AUTHOR_EDITOR_INPUT")
                before = history.read_bytes()
                terminal.command("!printf forbidden")
                assert history.read_bytes() == before, "read-only TUI mutated saved history"
                assert call(endpoint, "/snapshot")["state"]["active_run"] is None
            finally:
                terminal.close()
            terminal = Terminal(binary, reader_endpoint, env=env)
            terminal.wait("Read only")
            stop(reader, endpoint)
            try:
                terminal.wait("Disconnected")
            finally:
                terminal.close()
        finally:
            stop(reader, endpoint)
        missing = scratch / "missing-endpoint.json"
        terminal = Terminal(binary, missing, env=env)
        try:
            terminal.process.wait(timeout=5)
            terminal.read()
            assert b"No such file" in terminal.output or b"not found" in terminal.output, bytes(
                terminal.output
            )
        finally:
            terminal.close(expected_code=None, screen=False)
    write_receipt(
        {
            "status": "passed",
            "platform": sys.platform,
            "pty": "passed",
            "isolated_shared_host": True,
            "bracketed_paste": True,
            "resize": True,
            "detach_preserves_host": True,
            "saved_read_only_history": True,
            "endpoint_error_modes": True,
            "independent_editor": True,
            "independent_renderer": True,
            "independent_theme": True,
            "independent_frontend": True,
            "external_editor_success_and_failure": True,
            "suspend_resume_modes": True,
        },
        receipt,
    )


if __name__ == "__main__":
    main()
