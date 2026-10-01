#!/usr/bin/env python3
"""Installed real-pager evidence for recovery failures, readers and stopped hosts."""

import argparse
import json
import os
import signal
import subprocess
from pathlib import Path

from session_fixture import (
    Fixture,
    committed,
    completed_load,
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    trace = args.output / "wire.jsonl"
    checks = {}
    with Fixture(args.installation, args.output) as fixture:
        bad_directory = fixture.root / "not-a-directory"
        bad_directory.write_text("fixture")
        terminal = fixture.start(trace=trace, env={"EDEN_LEGACY_SESSION_DIRS": str(bad_directory)})
        terminal.command("!printf ORIGINAL_VIEW_MARKER")
        current = next((fixture.root / ".eden/sessions").glob("*.jsonl"))
        current_endpoint_path, current_endpoint = owner(fixture, current)
        committed(current_endpoint, "ORIGINAL_VIEW_MARKER")
        terminal.wait("Run (user)")
        damaged = current.with_name("damaged-copy.jsonl")
        damaged.write_bytes(current.read_bytes() + b"{invalid\n")
        terminal.send(b"UNSENT_DRAFT")
        terminal.send(b"\x12")  # Ctrl+R: native resume picker without consuming the draft.
        terminal.wait("Some history directories", seconds=30)
        fixture.capture("partial-directory")
        select(terminal, damaged, trace, success=False)
        terminal.wait("Couldn't load session")
        terminal.wait("UNSENT_DRAFT")
        terminal.wait("ORIGINAL_VIEW_MARKER")
        fixture.capture("failed-load-retained-view")
        checks["failed_load_preserves_original_view_and_unsent_draft"] = True
        checks["partial_directory_retains_valid_native_picker_rows"] = True
        terminal.send(b"\x15")
        terminal.command("!printf FAILURE_INPUT_USABLE")
        committed(current_endpoint, "FAILURE_INPUT_USABLE")
        terminal.wait("FAILURE_INPUT_USABLE")
        exited(terminal)
        original = current.read_bytes()
        assert writer_held(current)
        os.kill(current_endpoint["pid"], signal.SIGKILL)
        wait_until(lambda: process_gone(current_endpoint["pid"]))
        wait_until(lambda: not writer_held(current))
        assert any(
            json.loads(path.read_text()).get("endpoint") == str(current_endpoint_path)
            for path in (fixture.state / "live").glob("*.json")
        )
        terminal = fixture.start(trace=trace)
        terminal.command("/resume")
        select(terminal, current, trace)
        terminal.wait("FAILURE_INPUT_USABLE")
        new_endpoint_path, new_endpoint = owner(fixture, current)
        assert new_endpoint["session_id"] == current_endpoint["session_id"]
        assert new_endpoint["pid"] != current_endpoint["pid"]
        assert writer_held(current) and current.read_bytes() == original
        fixture.capture("crash-stale-registry-recovered")
        checks["crashed_owner_and_stale_registry_restore_same_history_without_replay"] = True
        terminal.command("/sessions")
        fixture.click(current.stem[:18])
        fixture.click("Stop live host")
        fixture.click("Stop confirmed host")
        terminal.wait("Confirm the selected host before stopping")
        assert new_endpoint_path.exists() and writer_held(current)
        fixture.click("Stop this host")
        terminal.send(b" ")
        terminal.wait("Stop this host: true")
        fixture.click("Stop confirmed host")
        terminal.wait("Host stopped.")
        assert not new_endpoint_path.exists() and not writer_held(current)
        wait_until(lambda: process_gone(new_endpoint["pid"]), terminal)
        fixture.capture("explicit-stop-cleanup")
        # Management remains usable after stopping the originally selected host.
        terminal.send(b"\x1b")
        terminal.command("/sessions")
        terminal.wait("Eden sessions")
        existing = set((fixture.root / ".eden/sessions").glob("*.jsonl"))
        offset = len(trace_rows(trace))
        fixture.click("New Session in this cwd")
        created_path = wait_until(
            lambda: next(
                (
                    path
                    for path in (fixture.root / ".eden/sessions").glob("*.jsonl")
                    if path not in existing
                ),
                None,
            ),
            terminal,
        )
        completed_load(terminal, created_path, trace, offset)
        terminal.command("!printf POST_STOP_INPUT_USABLE")
        _, created_endpoint = owner(fixture, created_path)
        committed(created_endpoint, "POST_STOP_INPUT_USABLE")
        terminal.wait("POST_STOP_INPUT_USABLE")
        checks["native_explicit_stop_waits_cleanup_and_management_survives"] = True
        exited(terminal)
        large = current.with_name("large-history.jsonl")
        source_records = records(current)
        large.write_bytes(current.read_bytes())
        start = source_records[-1]["sequence"]
        with large.open("a") as output:
            for index in range(80):
                record = {
                    "schema_version": 2,
                    "session_id": source_records[0]["session_id"],
                    "sequence": start + index + 1,
                    "parent_id": start + index,
                    "run_id": 0,
                    "kind": "user_message",
                    "branch": "main",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "text", "text": f"LARGE_HISTORY_{index:02d}"}],
                        "fixture_padding": "x" * 500000,
                    },
                }
                output.write(json.dumps({"schema_version": 2, "transaction": [record]}) + "\n")
        large_size = large.stat().st_size
        assert large_size >= 39_700_000
        # Same persistent ID in two distinct locators must not reuse the other history's host.
        terminal = fixture.start(trace=trace)
        terminal.command("/resume")
        load = select(terminal, large, trace)
        terminal.wait("LARGE_HISTORY_79", seconds=60)
        large_endpoint_path, large_endpoint = owner(fixture, large)
        assert large_endpoint_path != new_endpoint_path
        assert large_endpoint["session_id"] == current_endpoint["session_id"]
        ready_frame = wait_until(
            lambda: next(
                (
                    row
                    for row in trace_rows(trace)
                    if row.get("direction") == "frame"
                    and row.get("method") == "session/loaded"
                    and row.get("session") == load["reply"]["session"]
                    and row.get("ready") is True
                ),
                None,
            ),
            terminal,
        )
        phase_rows = [
            row
            for row in trace_rows(trace)
            if row.get("direction") == "phase" and row.get("session") == load["reply"]["session"]
        ]
        timings = {}
        for phase in ("connection", "snapshot", "projection"):
            starts = [
                row
                for row in phase_rows
                if row.get("phase") == phase and row.get("edge") == "start"
            ]
            ends = [
                row for row in phase_rows if row.get("phase") == phase and row.get("edge") == "end"
            ]
            timings[phase + "_seconds"] = sum(
                (end["time_ns"] - start["time_ns"]) / 1e9
                for start, end in zip(starts, ends, strict=True)
            )
        list_starts = [
            row
            for row in trace_rows(trace)
            if row.get("phase") == "directory" and row.get("edge") == "start"
        ]
        list_ends = [
            row
            for row in trace_rows(trace)
            if row.get("phase") == "directory" and row.get("edge") == "end"
        ]
        timings["directory_seconds"] = (list_ends[-1]["time_ns"] - list_starts[-1]["time_ns"]) / 1e9
        request = next(
            row
            for row in trace_rows(trace)
            if row.get("direction") == "in"
            and row.get("id") == load["request"]["id"]
            and row.get("session") == load["request"]["session"]
        )
        timings["request_to_written_operable_frame_seconds"] = (
            ready_frame["time_ns"] - request["time_ns"]
        ) / 1e9
        terminal.command("!printf LARGE_INPUT_USABLE")
        committed(large_endpoint, "LARGE_INPUT_USABLE")
        terminal.wait("LARGE_INPUT_USABLE")
        fixture.capture("large-history-ready")
        checks["large_history"] = {
            "bytes": large_size,
            "phase_timings": timings,
            "written_operable_frame": ready_frame,
            "synthetic_committed_user_messages_with_padding": 80,
            "load": load,
            "input_usable": True,
            "duplicate_persistent_id_uses_selected_locator": True,
        }
        exited(terminal)
        old_composition = json.loads(fixture.composition.read_text())
        old_composition["packages"] = [
            package
            for package in old_composition["packages"]
            if package["descriptor"]["package"] != "github-share"
        ]
        old_composition["roles"].pop("eden.share-target.v1", None)
        old_path = fixture.root / "old-composition.json"
        old_path.write_text(json.dumps(old_composition))
        incompatible = current.with_name("incompatible-history.jsonl")
        created = subprocess.run(
            [
                *fixture.arguments(old_path),
                "--session",
                str(incompatible),
                "models",
                "select",
                "fixture",
                "workflow",
            ],
            cwd=fixture.root,
            capture_output=True,
            text=True,
            check=False,
        )
        assert created.returncode == 0, created.stderr
        incompatible_before = incompatible.read_bytes()
        terminal = fixture.start(trace=trace)
        terminal.command("/resume")
        select(terminal, incompatible, trace)
        terminal.wait("Read-only history", seconds=30)
        terminal.wait("saved package binding differs")
        readers = [
            path
            for path in (fixture.state / "hosts").glob("*.json")
            if "startup" not in path.name
            and json.loads(path.read_text()).get("session_id")
            == records(incompatible)[0]["session_id"]
        ]
        reader_path = readers[0]
        reader = json.loads(reader_path.read_text())
        fixture.capture("binding-readonly")
        assert incompatible.read_bytes() == incompatible_before and not writer_held(incompatible)
        terminal.command("/sessions")
        fixture.click(incompatible.stem)
        fixture.click("Preview migrated copy")
        destination = fixture.root / "migrated-copy.jsonl"
        fixture.click("New copy path")
        terminal.send(b"\x15" + str(destination).encode())
        fixture.click("Preview copy")
        terminal.wait("Review session copy")
        assert not destination.exists()
        fixture.capture("migration-preview")
        fixture.click("Create this copy and open it")
        terminal.wait("Empty session", seconds=30)
        terminal.command("!printf MIGRATED_INPUT_USABLE")
        migrated_path, migrated = owner(fixture, destination)
        committed(migrated, "MIGRATED_INPUT_USABLE")
        assert incompatible.read_bytes() == incompatible_before
        assert migrated["session_id"] != reader["session_id"]
        fixture.capture("migrated-copy-ready")
        exited(terminal)
        assert not reader_path.exists()
        wait_until(lambda: process_gone(reader["pid"]))
        checks["binding_incompatible_readonly_and_explicit_migration_preserve_source"] = True
        checks["frontend_exit_waits_owned_reader_cleanup"] = True
        borrowed_path = fixture.root / "borrowed-reader.json"
        borrowed = subprocess.Popen(
            [*fixture.arguments(), "read", str(incompatible), "--endpoint", str(borrowed_path)],
            cwd=fixture.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={**os.environ, "EDEN_TUI_STATE_DIR": str(fixture.state)},
        )
        borrowed_endpoint = None
        try:
            borrowed_endpoint = probe.streaming.live.ready(borrowed, borrowed_path)
            terminal = fixture.start(
                extra=["live-tui", "--endpoint", str(borrowed_path)], trace=trace
            )
            terminal.wait("Read-only history")
            exited(terminal)
            assert borrowed.poll() is None and borrowed_path.exists()
            assert probe.streaming.live.call(borrowed_endpoint, "/snapshot")["state"]["read_only"]
            checks["borrowed_reader_detaches_without_shutdown"] = True
        finally:
            if borrowed_endpoint is not None:
                probe.streaming.live.call(borrowed_endpoint, "/shutdown", {})
                assert borrowed.wait(timeout=15) == 0
            elif borrowed.poll() is None:
                borrowed.kill()
                borrowed.wait()
        nested = fixture.root / "nested-project"
        nested.mkdir()
        terminal = probe.Terminal(
            fixture.installation / "bin/eden",
            fixture.endpoint_file,
            width=110,
            height=40,
            command=[
                str(fixture.installation / "bin/eden"),
                "--composition",
                "composition.json",
                "--global-dir",
                "global",
                "--cwd",
                str(nested),
                "--no-session",
                "--no-trust-project",
                "--offline-startup",
            ],
            env={"EDEN_TUI_STATE_DIR": str(fixture.state), "EDEN_FRONTEND_TRACE": str(trace)},
        )
        fixture.terminal = terminal
        terminal.wait("workflow", seconds=30)
        assert not list((nested / ".eden/sessions").glob("*.jsonl"))
        terminal.command("/sessions")
        fixture.click("New Session in this cwd")
        terminal.wait("Empty session", seconds=30)
        assert len(list((nested / ".eden/sessions").glob("*.jsonl"))) == 1
        checks["relative_launch_configuration_survives_frontend_cwd_change"] = True
        checks["no_session_start_and_explicit_persistent_new_are_distinct"] = True
        exited(terminal)
        assert len(fixture.provider.requests) == 0
        assert not fixture.business.calls, fixture.business.calls
        checks["grok_business_http_calls"] = 0
        observed = trace_rows(trace)
        directory_starts = [
            row
            for row in observed
            if row.get("phase") == "directory" and row.get("edge") == "start"
        ]
        directory_ends = [
            row for row in observed if row.get("phase") == "directory" and row.get("edge") == "end"
        ]
        early_frames = [
            frame
            for start, end in zip(directory_starts, directory_ends, strict=True)
            for frame in observed
            if frame.get("direction") == "frame"
            and frame.get("method") == "directory/accepted"
            and frame.get("count", 0) > 0
            and frame.get("row")
            and isinstance(start.get("picker"), dict)
            and start["picker"] == end["picker"]
            and frame.get("host") == start["picker"]["host"]
            and frame.get("generation") == start["picker"]["generation"]
            and frame.get("seq") == start["picker"]["seq"]
            and start["time_ns"] < frame["time_ns"] < end["time_ns"]
        ]
        assert early_frames, "No usable directory row written before scan completion"
        checks["progressive_directory_written_before_scan_complete"] = early_frames[0]
        checks["provider_requests"] = 0
        (args.output / "summary.json").write_text(json.dumps(checks, indent=2) + "\n")


if __name__ == "__main__":
    main()
