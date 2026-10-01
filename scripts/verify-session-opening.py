#!/usr/bin/env python3
"""Hold a real history load, cancel it, and consume its late result without losing the old view."""

import argparse
import importlib
import json
from pathlib import Path

from install import package, target
from session_fixture import Fixture, exited, select, trace_rows, wait_until

ResolveGate = importlib.import_module("verify-session-admission").ResolveGate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--author", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    trace = args.output / "wire.jsonl"
    with Fixture(args.installation, args.output) as fixture, ResolveGate() as gate:
        composition = json.loads(fixture.composition.read_text())
        model = next(
            p for p in composition["packages"] if p["descriptor"]["package"] == "model-access"
        )
        target_path = fixture.root / "target.json"
        target_path.write_text(json.dumps(model["config"]["catalog"]["models"][0]))
        enable = fixture.root / "catalog-gate"
        composition["packages"].append(
            package(
                "model-services",
                [
                    "eden.coding-provider.v1",
                    "eden.model-catalog.v1",
                    "eden.model-manager.v1",
                    "eden.credential-source.v1",
                ],
                str(args.author.resolve()),
                target(),
                {"target_path": str(target_path), "catalog_gate": str(enable)},
            )
        )
        composition["roles"]["eden.model-catalog.v1"] = "model-services"
        config = fixture.root / "gated-model-list.json"
        config.write_text(json.dumps(composition))
        terminal = fixture.start(composition=config, trace=trace)
        terminal.command("!printf LOAD_TARGET_CONTENT")
        terminal.wait("LOAD_TARGET_CONTENT")
        destination = wait_until(
            lambda: next((fixture.root / ".eden/sessions").glob("*.jsonl"), None), terminal
        )
        exited(terminal)
        terminal = fixture.start(trace=trace)
        terminal.send(b"UNSENT_OLD_DRAFT")
        terminal.send(b"\x12")
        terminal.wait("Resume session")
        terminal.send(b"/" + destination.stem.encode())
        terminal.wait(destination.stem[:15])
        offset = len(trace_rows(trace))
        enable.write_text(gate.address)
        terminal.send(b"\r")
        assert gate.arrived.wait(15) and gate.error is None, gate.error
        load = next(
            row
            for row in trace_rows(trace)[offset:]
            if row.get("direction") == "in" and row.get("method") == "session/load"
        )
        wait_until(
            lambda: any(
                row.get("direction") == "frame"
                and row.get("method") == "session/update"
                and row.get("session") == load["session"]
                for row in trace_rows(trace)[offset:]
            ),
            terminal,
        )
        terminal.drain()
        fixture.capture("pending-load")
        assert "UNSENT_OLD_DRAFT" in "\n".join(terminal.display.splitlines()[-9:]), terminal.display
        assert "LOAD_TARGET_CONTENT" not in terminal.display, terminal.display
        terminal.send(b"\x1b")
        wait_until(
            lambda: any(
                row.get("direction") == "frame"
                and row.get("method") == "session/load_cancelled"
                and row.get("session") == load["session"]
                for row in trace_rows(trace)[offset:]
            ),
            terminal,
        )
        terminal.send(b"_STILL_EDITABLE")
        terminal.wait("UNSENT_OLD_DRAFT_STILL_EDITABLE")
        enable.unlink()
        gate.reply = "accept"
        gate.release.set()
        wait_until(
            lambda: any(
                row.get("direction") == "frame"
                and row.get("method") == "session/load_discarded"
                and row.get("session") == load["session"]
                for row in trace_rows(trace)[offset:]
            ),
            terminal,
        )
        terminal.drain()
        assert "UNSENT_OLD_DRAFT_STILL_EDITABLE" in "\n".join(terminal.display.splitlines()[-9:])
        assert "LOAD_TARGET_CONTENT" not in terminal.display
        fixture.capture("cancelled-late-result")
        terminal.send(b"\x12")
        terminal.wait("Resume session")
        select(terminal, destination, trace)
        terminal.wait("LOAD_TARGET_CONTENT")
        assert not fixture.provider.requests
        exited(terminal)
    (args.output / "summary.json").write_text(
        json.dumps(
            {
                "old_view_retained_until_ready": True,
                "cancel_preserves_editable_draft": True,
                "late_result_consumed_without_switch": True,
                "reopen_ready_target": True,
            },
            indent=2,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
