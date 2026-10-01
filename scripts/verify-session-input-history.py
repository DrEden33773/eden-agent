#!/usr/bin/env python3
"""Scope prompt recall to the restored view even before its activation has settled."""

import argparse
import json
from pathlib import Path

from session_fixture import Fixture, exited, ready_view, select, trace_rows, wait_until


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    trace = args.output / "wire.jsonl"
    with Fixture(args.installation, args.output) as fixture:
        terminal = fixture.start(trace=trace)
        terminal.command("FIRST_HISTORY_PROMPT")
        terminal.wait("STREAM_FINISHED", seconds=30)
        first = next((fixture.root / ".eden/sessions").glob("*.jsonl"))
        offset = len(trace_rows(trace))
        terminal.command("/new")
        ready_view(terminal, trace, offset)
        terminal.command("SECOND_HISTORY_PROMPT")
        terminal.wait("STREAM_FINISHED", seconds=30)
        assert len(fixture.provider.requests) == 2
        offset = len(trace_rows(trace))
        terminal.command("/resume")
        load = select(terminal, first, trace)
        ready_view(terminal, trace, offset)

        def history_request():
            return next(
                (
                    row
                    for row in trace_rows(trace)[offset:]
                    if row.get("direction") == "in"
                    and row.get("method") == "session/prompt_history"
                    and row["id"] > load["request"]["id"]
                ),
                None,
            )

        request = wait_until(history_request, terminal)
        observation = {
            "loaded_view": load["reply"]["session"],
            "requested_view": request["session"],
        }
        (args.output / "routing.json").write_text(json.dumps(observation, indent=2) + "\n")
        assert request["session"] == load["reply"]["session"], observation
        reply = wait_until(
            lambda: next(
                (
                    row
                    for row in trace_rows(trace)
                    if row.get("direction") == "out" and row.get("id") == request["id"]
                ),
                None,
            ),
            terminal,
        )
        assert reply["error"] is None, reply
        terminal.send(b"\x1b[A")
        wait_until(
            lambda: "FIRST_HISTORY_PROMPT" in "\n".join(terminal.display.splitlines()[-7:]),
            terminal,
        )
        assert "SECOND_HISTORY_PROMPT" not in "\n".join(terminal.display.splitlines()[-7:])
        terminal.send(b"_EDITED")
        wait_until(
            lambda: "FIRST_HISTORY_PROMPT_EDITED" in "\n".join(terminal.display.splitlines()[-7:]),
            terminal,
        )
        fixture.capture("recalled-target-input")
        assert len(fixture.provider.requests) == 2 and not fixture.business.calls
        terminal.send(b"\x15")
        exited(terminal)
    (args.output / "summary.json").write_text(
        json.dumps(
            {
                "input_history_scoped_to_loaded_view": True,
                "recall_uses_target_prompt_without_new_provider_work": True,
            },
            indent=2,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
