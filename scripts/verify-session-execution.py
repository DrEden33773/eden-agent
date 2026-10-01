#!/usr/bin/env python3
"""Prove native frontend detach and target switching preserve host-owned accepted work."""

import argparse
import json
from pathlib import Path

from session_fixture import (
    Fixture,
    committed,
    exited,
    owner,
    probe,
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
    with Fixture(args.installation, args.output) as fixture:
        terminal = fixture.start(trace=trace)
        fixture.provider.allow.clear()
        terminal.command("WORK_AFTER_DETACH")
        assert fixture.provider.started.wait(timeout=10)
        history = next((fixture.root / ".eden/sessions").glob("*.jsonl"))
        endpoint_path, endpoint = owner(fixture, history)
        active = probe.streaming.live.call(endpoint, "/tui/snapshot")["state"]["active_run"]
        assert active is not None
        exited(terminal)
        detached = probe.streaming.live.call(endpoint, "/tui/snapshot")
        assert detached["state"]["active_run"] == active and writer_held(history)
        fixture.provider.allow.set()
        settled = probe.streaming.live.wait_for(
            endpoint,
            lambda frame: frame["state"]["active_run"] is None
            and any(
                "STREAM_FINISHED" in json.dumps(record["payload"]) for record in frame["history"]
            ),
        )
        before_restore = history.read_bytes()
        assert len(fixture.provider.requests) == 1
        terminal = fixture.start(trace=trace)
        # A second saved task is created through the real composer before testing view isolation.
        terminal.command("!printf SECOND_VIEW_TASK")
        terminal.wait("SECOND_VIEW_TASK")
        empty = wait_until(
            lambda: next(
                (
                    path
                    for path in (fixture.root / ".eden/sessions").glob("*.jsonl")
                    if path != history
                ),
                None,
            ),
            terminal,
        )
        terminal.command("/resume")
        select(terminal, history, trace)
        terminal.wait("STREAM_FINISHED")
        assert history.read_bytes() == before_restore and len(fixture.provider.requests) == 1
        fixture.provider.started.clear()
        fixture.provider.done.clear()
        fixture.provider.allow.clear()
        # One agent chunk contains the final marker, making its frame unambiguous.
        fixture.provider.mode = "single"
        terminal.command("LATE_PREVIOUS_TARGET_WORK")
        assert fixture.provider.started.wait(timeout=10)
        active_previous = probe.streaming.live.call(endpoint, "/tui/snapshot")["state"][
            "active_run"
        ]
        terminal.send(b"\x12")
        selected_load = select(terminal, empty, trace)
        terminal.wait("SECOND_VIEW_TASK")
        terminal.send(b"NEW_TARGET_DRAFT")
        offset = len(trace.read_text().splitlines())
        fixture.provider.allow.set()
        probe.streaming.live.wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
        consumed = wait_until(
            lambda: next(
                (
                    row
                    for row in trace_rows(trace)[offset:]
                    if row.get("direction") == "frame"
                    and row.get("method") == "session/update"
                    and row.get("update") == "agent_message_chunk"
                    and row.get("session", "").startswith(f"history:{history}::view-")
                    and row.get("run") == active_previous
                    and row.get("replay") is False
                    and row.get("view") == selected_load["reply"]["session"]
                ),
                None,
            ),
            terminal,
        )
        outgoing = [
            row
            for row in trace_rows(trace)[offset:]
            if row.get("direction") == "event"
            and row.get("kind") == "Update"
            and row.get("update") == "agent_message_chunk"
            and row.get("session") == consumed["session"]
            and row.get("run") == active_previous
            and row.get("replay") is False
        ]
        assert len(outgoing) == 1, "The marker must identify exactly one agent update"
        terminal.drain()
        assert "STREAM_FINISHED" not in terminal.display
        assert "NEW_TARGET_DRAFT" in terminal.display
        fixture.capture("late-old-events-isolated")
        terminal.send(b"\x15")
        terminal.command("!printf SELECTED_TARGET_INPUT_USABLE")
        _, selected_endpoint = owner(fixture, empty)
        committed(selected_endpoint, "SELECTED_TARGET_INPUT_USABLE")
        terminal.wait("SELECTED_TARGET_INPUT_USABLE")
        fixture.capture("selected-target-ready")
        exited(terminal)
        assert len(fixture.provider.requests) == 2 and not fixture.business.calls
        summary = {
            "accepted_work_survives_ctrl_d_detach": True,
            "restored_completed_work_without_replay": True,
            "late_previous_target_events_do_not_pollute_new_view_or_draft": True,
            "matching_consumed_and_written_frame": consumed,
            "selected_target_input_usable": True,
            "provider_requests": 2,
            "grok_business_http_calls": 0,
            "host_pid_unchanged": endpoint["pid"],
            "settled_records": len(settled["history"]),
            "endpoint_locator": str(endpoint_path),
        }
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
