#!/usr/bin/env python3
"""Exercise draft admission and reversible history management through the installed terminal."""

import argparse
import json
import subprocess
from pathlib import Path

from session_fixture import (
    Fixture,
    exited,
    owner,
    probe,
    process_gone,
    records,
    select,
    trace_rows,
    wait_until,
    writer_held,
)


def histories(fixture):
    return sorted(
        path for path in (fixture.root / ".eden/sessions").glob("*.jsonl") if path.is_file()
    )


def registrations(fixture):
    return [
        json.loads(path.read_text())
        for path in (fixture.state / "live").glob("*.json")
        if json.loads(path.read_text()).get("history") != str(fixture.history)
    ]


def focus(terminal, path):
    """Filter the native picker, then leave its search editor before invoking a row action."""
    terminal.send(b"/" + path.stem.encode())
    terminal.wait(path.stem[:18])
    terminal.send(b"\x1b[B")


def gone(terminal, label):
    wait_until(lambda: label not in terminal.display, terminal)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    trace = args.output / "wire.jsonl"
    checks = {}
    with Fixture(args.installation, args.output) as fixture:
        for _ in range(2):
            terminal = fixture.start(trace=trace)
            assert not histories(fixture)
            terminal.command("/resume")
            terminal.wait("Resume session")
            assert "Empty session" not in terminal.display
            terminal.send(b"\x1b")
            gone(terminal, "Resume session")
            exited(terminal)
            assert not histories(fixture)
            assert not registrations(fixture)
        checks["empty_launch_browse_exit_never_saves_history_or_leaks_draft_host"] = True

        terminal = fixture.start(trace=trace)
        destination = Path(registrations(fixture)[0]["history"])
        destination.mkdir()
        offset = len(trace_rows(trace))
        terminal.command("FIRST_WORK_MUST_BE_SAVED")
        wait_until(
            lambda: any(
                row.get("direction") == "out" and row.get("error")
                for row in trace_rows(trace)[offset:]
            ),
            terminal,
        )
        assert not fixture.provider.requests
        assert not histories(fixture)
        terminal.wait("FIRST_WORK_MUST_BE_SAVED")
        fixture.capture("failed-admission")
        assert "FIRST_WORK_MUST_BE_SAVED" in "\n".join(terminal.display.splitlines()[-9:])
        destination.rmdir()
        terminal.send(b"\r")
        assert fixture.provider.started.wait(10)
        # The provider has received its first request; all initial records and intent must
        # already be durable at this observable external-effect boundary.
        durable = records(destination)
        assert durable[0]["kind"] == "session"
        assert any(
            record["kind"] == "work_admitted"
            and "FIRST_WORK_MUST_BE_SAVED" in json.dumps(record["payload"])
            for record in durable
        )
        terminal.wait("STREAM_FINISHED", seconds=30)
        _, endpoint = owner(fixture, destination)
        probe.streaming.live.wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
        checks["failed_admission_preserves_input_and_has_no_provider_effect"] = True
        checks["retry_publishes_initial_history_and_intent_before_provider_request"] = True

        before = destination.read_bytes()
        terminal.command("/sessions")
        focus(terminal, destination)
        terminal.send(b"d")
        terminal.wait("Move session to Trash?")
        terminal.send(b"\x1b")
        terminal.wait("Resume session")
        assert destination.read_bytes() == before and writer_held(destination)
        checks["cancel_removal_keeps_records_and_live_owner"] = True

        terminal.send(b"r")
        terminal.wait("Rename session")
        fixture.click("Name")
        terminal.send("同名任务 · 中文".encode())
        fixture.click("Save name")
        terminal.wait("Resume session")
        terminal.wait("同名任务")
        assert records(destination)[-1]["payload"].get("name") == "同名任务 · 中文" or any(
            record["kind"] == "session_metadata" and record["payload"]["name"] == "同名任务 · 中文"
            for record in records(destination)
        )
        fixture.capture("renamed-history")
        checks["native_rename_and_filtered_list_refresh"] = True
        # The original picker preserves its filter; Esc first clears that filter.
        terminal.send(b"\x1b")
        terminal.wait("/ to search")
        terminal.send(b"\x1b")
        gone(terminal, "Resume session")
        terminal.send("UNSENT_保留输入".encode())
        terminal.send(b"\x12")
        focus(terminal, destination)
        terminal.send(b"d")
        terminal.wait("Close session and move to Trash")
        fixture.click("Close session and move to Trash")
        wait_until(lambda: not destination.exists(), terminal)
        terminal.wait("UNSENT_保留输入", seconds=30)
        gone(terminal, "Move session to Trash?")
        terminal.drain()
        assert "UNSENT_保留输入" in "\n".join(terminal.display.splitlines()[-9:])
        retained = next(destination.parent.glob(".trash/*/history.jsonl"))
        retained_bytes = retained.read_bytes()
        assert not writer_held(destination)
        fixture.capture("current-removal-preserves-input")
        checks["current_idle_removal_stops_owner_and_preserves_unsent_input"] = True
        terminal.send(b"\x15")
        exited(terminal)

        terminal = fixture.start(trace=trace)
        terminal.command("/resume")
        terminal.send(b"f")
        terminal.wait("All saved")
        terminal.send(b"f")
        terminal.wait("Trash")
        terminal.wait("同名任务")
        fixture.capture("trash-after-restart")
        terminal.send(b"\r")
        terminal.wait("Restore session")
        fixture.click("Restore session")
        wait_until(destination.is_file, terminal)
        terminal.wait("Resume session")
        assert destination.read_bytes() == retained_bytes and not retained.exists()
        terminal.send(b"f")
        terminal.wait("Recent")
        select(terminal, destination, trace)
        terminal.wait("STREAM_FINISHED")
        assert len(fixture.provider.requests) == 1
        checks["trash_reopens_and_restores_exact_records_through_native_picker"] = True

        # A cold rename uses the same API as the TUI and must not start a new host.
        _, restored_owner = owner(fixture, destination)
        probe.streaming.live.call(
            restored_owner, "/shutdown", {"session_id": restored_owner["session_id"]}
        )
        wait_until(lambda: not writer_held(destination), terminal)
        count = len(list((fixture.state / "hosts").glob("*.json")))
        renamed = subprocess.run(
            [
                *fixture.arguments(),
                "session",
                "metadata",
                str(destination),
                "--name",
                "Cold rename",
                "--tag",
                "kept",
            ],
            cwd=fixture.root,
            capture_output=True,
            text=True,
            check=False,
        )
        assert renamed.returncode == 0, renamed.stderr
        assert len(list((fixture.state / "hosts").glob("*.json"))) == count
        checks["cold_cli_metadata_uses_writer_lock_without_plugin_host"] = True
        exited(terminal)

        terminal = fixture.start(trace=trace)
        terminal.command("/resume")
        select(terminal, destination, trace)
        _, running_owner = owner(fixture, destination)
        terminal.command("/resume")
        focus(terminal, destination)
        terminal.send(b"d")
        terminal.wait("Close session and move to Trash")
        fixture.provider.mode = "blocking_tool"
        fixture.provider.tool_round = 0
        # A second consumer starts work after the visible idle confirmation was prepared.
        admitted = probe.streaming.live.call(
            running_owner,
            "/prompt",
            {
                "session_id": running_owner["session_id"],
                "request_id": "other-client-work",
                "text": "RUNNING_DELETE_TASK",
            },
        )
        parent_file = fixture.root / "parent.pid"
        child_file = fixture.root / "child.pid"
        wait_until(lambda: parent_file.exists() and child_file.exists(), terminal)
        parent, child = int(parent_file.read_text()), int(child_file.read_text())
        fixture.click("Close session and move to Trash")
        terminal.wait("history changed")
        assert destination.exists() and writer_held(destination)
        assert (
            probe.streaming.live.call(running_owner, "/tui/snapshot")["state"]["active_run"]
            == admitted["run_id"]
        )
        assert not process_gone(parent) and not process_gone(child)
        checks["stale_idle_confirmation_cannot_stop_newly_accepted_work"] = True
        terminal.send(b"\x1b")
        terminal.wait("Resume session")
        terminal.send(b"d")
        terminal.wait("Stop task and move to Trash")
        fixture.capture("running-removal-confirmation")
        fixture.click("Stop task and move to Trash")
        wait_until(
            lambda: not destination.exists() and process_gone(parent) and process_gone(child),
            terminal,
        )
        gone(terminal, "Move session to Trash?")
        assert not writer_held(destination)
        checks["explicit_running_removal_waits_tool_tree_cleanup_and_trashes_history"] = True
        exited(terminal)
        assert not fixture.business.calls
        checks["provider_requests"] = len(fixture.provider.requests)
        checks["grok_business_http_calls"] = 0
    (args.output / "summary.json").write_text(json.dumps(checks, indent=2) + "\n")


if __name__ == "__main__":
    main()
