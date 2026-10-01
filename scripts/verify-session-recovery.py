#!/usr/bin/env python3
"""Exercise installed Eden, the real pager and native picker across Ctrl+D detach."""

import argparse
import json
import time
from pathlib import Path

from session_fixture import (
    Fixture,
    committed,
    probe,
    process_gone,
    trace_rows,
    wait_until,
    writer_held,
)


def exited(terminal):
    terminal.send(b"\x04")
    terminal.send(b"\x04")
    deadline = time.monotonic() + 15
    while terminal.process.poll() is None and time.monotonic() < deadline:
        terminal.read(0.1)
    assert terminal.process.poll() == 0, terminal.display
    terminal.close(screen=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture:
        fixture.mutate(
            "/models/catalog",
            {
                "request": {
                    "action": "set_default",
                    "selection": {"provider": "fixture", "model": "workflow", "thinking": "off"},
                }
            },
        )

        def start():
            terminal = fixture.start(trace=args.output / "wire.jsonl")
            fixture.terminal = terminal
            terminal.wait("workflow", seconds=30)
            return terminal

        def loaded(terminal, path, count):
            expected = f"history:{path.resolve()}::view-"
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                terminal.read(0.1)
                rows = trace_rows(args.output / "wire.jsonl")[count:]
                requests = [
                    row
                    for row in rows
                    if row["direction"] == "in"
                    and row["method"] == "session/load"
                    and isinstance(row["session"], str)
                    and row["session"].startswith(expected)
                ]
                if (
                    requests
                    and any(
                        row["direction"] == "out"
                        and row["id"] == requests[-1]["id"]
                        and row["error"] is None
                        for row in rows
                    )
                    and "Loading session" not in terminal.display
                ):
                    return
            raise AssertionError(f"No completed load for {expected}: {terminal.display}")

        def select(terminal, path):
            count = len((args.output / "wire.jsonl").read_text().splitlines())
            terminal.send(b"/")
            terminal.send(path.stem.encode())
            terminal.wait(path.stem[:15])
            terminal.send(b"\r")
            loaded(terminal, path, count)

        def capture(name, terminal):
            (args.output / f"{name}.txt").write_text(terminal.display)
            (args.output / f"{name}.ansi").write_bytes(terminal.output)

        terminal = start()
        terminal.command("!printf OLD_NONEMPTY_MARKER")
        terminal.wait("OLD_NONEMPTY_MARKER")
        old = next((fixture.root / ".eden/sessions").glob("*.jsonl"))
        old_info = fixture.call("/manage/info", {"path": str(old)})
        old_endpoint_path = Path(old_info["owner"])
        old_endpoint = json.loads(old_endpoint_path.read_text())
        fixture.children.append(old_endpoint)
        committed(old_endpoint, "OLD_NONEMPTY_MARKER")
        terminal.wait("Run (user)")
        capture("original", terminal)
        exited(terminal)
        before = probe.streaming.live.call(old_endpoint, "/tui/snapshot")
        original_bytes = old.read_bytes()
        assert writer_held(old) and not process_gone(old_endpoint["pid"])
        terminal = start()
        new = next(
            path for path in (fixture.root / ".eden/sessions").glob("*.jsonl") if path != old
        )
        for registration in (fixture.root / "host-state/live").glob("*.json"):
            fixture.children.append(
                json.loads(Path(json.loads(registration.read_text())["endpoint"]).read_text())
            )
        terminal.command("/resume")
        terminal.wait("OLD_NONEMPTY_MARKER", seconds=30)
        terminal.wait("Current empty session")
        capture("picker", terminal)
        select(terminal, new)
        terminal.wait("Empty session", seconds=30)
        assert "OLD_NONEMPTY_MARKER" not in terminal.display
        capture("empty", terminal)
        terminal.command("/resume")
        terminal.wait("OLD_NONEMPTY_MARKER")
        select(terminal, old)
        terminal.wait("Run (user)", seconds=30)
        terminal.wait("OLD_NONEMPTY_MARKER")
        assert old.read_bytes() == original_bytes
        live_info = fixture.call("/manage/info", {"path": str(old)})
        assert live_info["owner"] == str(old_endpoint_path)
        terminal.command("!printf RECOVERED_INPUT_USABLE")
        probe.streaming.live.wait_for(
            old_endpoint,
            lambda frame: any(
                record["kind"] == "user_shell"
                and "RECOVERED_INPUT_USABLE" in record["payload"].get("command", "")
                for record in frame["history"]
            ),
        )
        terminal.wait("RECOVERED_INPUT_USABLE")
        capture("live", terminal)
        after = probe.streaming.live.call(old_endpoint, "/tui/snapshot")
        assert after["state"]["session_id"] == before["state"]["session_id"]
        assert len(fixture.provider.requests) == 0
        exited(terminal)
        probe.streaming.live.call(
            old_endpoint, "/shutdown", {"session_id": old_endpoint["session_id"]}
        )
        # Registry retirement and lock release are separate from shutdown acceptance.
        lock_path = Path(str(old) + ".lock")
        import fcntl

        with lock_path.open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            fcntl.flock(lock, fcntl.LOCK_UN)
        wait_until(lambda: process_gone(old_endpoint["pid"]))
        stopped_bytes = old.read_bytes()
        terminal = start()
        terminal.command("/resume")
        terminal.wait("OLD_NONEMPTY_MARKER", seconds=30)
        select(terminal, old)
        assert old.read_bytes() == stopped_bytes and writer_held(old)
        terminal.wait("RECOVERED_INPUT_USABLE", seconds=30)
        terminal.command("!printf STOPPED_INPUT_USABLE")
        restored_before_input = fixture.call("/manage/info", {"path": str(old)})
        restored_endpoint = json.loads(Path(restored_before_input["owner"]).read_text())
        committed(restored_endpoint, "STOPPED_INPUT_USABLE")
        terminal.wait("STOPPED_INPUT_USABLE")
        capture("stopped", terminal)
        restored = fixture.call("/manage/info", {"path": str(old)})
        assert restored["session_id"] == old_info["session_id"]
        assert restored["owner"] != str(old_endpoint_path)
        assert len(fixture.provider.requests) == 0
        exited(terminal)
        assert not fixture.business.calls, fixture.business.calls
        assert restored_endpoint["pid"] != old_endpoint["pid"]
        (args.output / "summary.json").write_text(
            json.dumps(
                {
                    "ctrl_d_twice": True,
                    "no_rename": True,
                    "native_empty_and_nonempty_selection": True,
                    "live_owner_identity": old_info["session_id"],
                    "stopped_new_host_same_identity": restored["session_id"],
                    "provider_requests": len(fixture.provider.requests),
                    "live_pid": old_endpoint["pid"],
                    "restored_pid": restored_endpoint["pid"],
                    "history_bytes_unchanged_by_each_restore": True,
                    "writer_owned_by_selected_host": True,
                    "grok_business_http_calls": len(fixture.business.calls),
                },
                indent=2,
            )
            + "\n"
        )


if __name__ == "__main__":
    main()
