#!/usr/bin/env python3
"""Exit after a new host publishes identity but before its adapter finishes the first snapshot."""

import argparse
import importlib
import json
from pathlib import Path

from install import package, target
from session_fixture import Fixture, exited, probe, process_gone, wait_until, writer_held

ResolveGate = importlib.import_module("verify-session-admission").ResolveGate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--author", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture, ResolveGate() as gate:
        composition = json.loads(fixture.composition.read_text())
        enable = fixture.root / "snapshot-gate.json"
        composition["packages"].append(
            package(
                "coding-replacements",
                [
                    "eden.coding-provider.v1",
                    "eden.coding-context.v2",
                    "eden.coding-tool.v1",
                    "eden.session-store.v2",
                    "eden.record-interpreter.v1",
                    "eden.state-migrator.v1",
                ],
                str(args.author.resolve()),
                target(),
                {"snapshot_gate": str(enable)},
            )
        )
        composition["roles"]["eden.session-store.v2"] = "coding-replacements"
        config = fixture.root / "gated-snapshot.json"
        config.write_text(json.dumps(composition))
        terminal = fixture.start(composition=config, trace=args.output / "wire.jsonl")
        initial = next(
            json.loads(p.read_text())
            for p in (fixture.state / "hosts").glob("*.json")
            if "address" in json.loads(p.read_text())
        )
        enable.write_text(
            json.dumps(
                {
                    "address": gate.address,
                    "hosts": str(fixture.state / "hosts"),
                    "except_pid": initial["pid"],
                }
            )
        )
        terminal.command("/new")
        assert gate.arrived.wait(15) and gate.error is None, gate.error
        child_path = next(
            p
            for p in (fixture.state / "hosts").glob("*.json")
            if json.loads(p.read_text()).get("pid") == gate.run
        )
        child = json.loads(child_path.read_text())
        registration_path = next(
            p
            for p in (fixture.state / "live").glob("*.json")
            if json.loads(p.read_text()).get("endpoint") == str(child_path)
        )
        history = Path(json.loads(registration_path.read_text())["history"])
        state = probe.streaming.live.call(child, "/snapshot")["state"]
        assert state["draft"] and state["active_run"] is None and writer_held(history)
        exited(terminal)
        wait_until(
            lambda: process_gone(child["pid"])
            and not child_path.exists()
            and not registration_path.exists()
            and not writer_held(history),
            seconds=10,
        )
        observation = {
            "process_gone": process_gone(child["pid"]),
            "endpoint_exists": child_path.exists(),
            "registration_exists": registration_path.exists(),
            "writer_held": writer_held(history),
            "history_exists": history.exists(),
        }
        (args.output / "handoff.json").write_text(json.dumps(observation, indent=2) + "\n")
        assert observation == {
            "process_gone": True,
            "endpoint_exists": False,
            "registration_exists": False,
            "writer_held": False,
            "history_exists": False,
        }, observation
        gate.release.set()
        assert not fixture.provider.requests
    (args.output / "summary.json").write_text(
        json.dumps({"cancelled_adapter_preparation_cleans_owned_host": True}) + "\n"
    )


if __name__ == "__main__":
    main()
