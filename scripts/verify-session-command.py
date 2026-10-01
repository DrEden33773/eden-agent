#!/usr/bin/env python3
"""An independent command reads the saved original request before its external effect."""

import argparse
import json
from pathlib import Path

from install import package, target
from session_fixture import Fixture, exited, probe, records, wait_until


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--author", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture:
        composition = json.loads(fixture.composition.read_text())
        composition["packages"].append(
            package(
                "service-b",
                [
                    "example.client.v1",
                    "example.commands.v1",
                    "example.command.v1",
                    "example.input.v1",
                    "example.tool.v1",
                ],
                str(args.author.resolve()),
                target(),
                {"intent_probe": True},
            )
        )
        composition["roles"]["example.commands.v1"] = "service-b"
        composition["roles"]["example.command.v1"] = "service-b"
        for item in composition["packages"]:
            if item["descriptor"]["package"] == "contributions":
                item["config"] = {
                    "commands": [
                        {"catalog": "example.commands.v1", "execute": "example.command.v1"}
                    ]
                }
        config = fixture.root / "command-probe.json"
        config.write_text(json.dumps(composition))
        terminal = fixture.start(composition=config, trace=args.output / "wire.jsonl")
        registration = next(
            json.loads(p.read_text())
            for p in (fixture.state / "live").glob("*.json")
            if json.loads(p.read_text()).get("history") != str(fixture.history)
        )
        history = Path(registration["history"])
        endpoint = json.loads(Path(registration["endpoint"]).read_text())
        effect = fixture.root / "command-effect.jsonl"
        arguments = {
            "value": 17,
            "text": "完整参数 · keep me",
            "nested": {"enabled": True},
            "history": str(history),
            "receipt": str(effect),
        }
        reply = probe.streaming.live.call(
            endpoint,
            "/command",
            {
                "request_id": "first-command-intent",
                "name": "example.compute",
                "arguments": arguments,
            },
        )
        settled = probe.streaming.live.call(endpoint, "/terminal", {"run_id": reply["run_id"]})
        assert settled["outcome"]["status"] == "completed", settled
        wait_until(effect.is_file, terminal)
        observed = records(effect)
        intent = next(r["payload"]["intent"] for r in observed if r["kind"] == "work_admitted")
        (args.output / "command-intent.json").write_text(json.dumps(intent, indent=2) + "\n")
        assert intent["name"] == "example.compute"
        assert intent.get("arguments") == arguments, intent
        assert intent["cwd"] == str(fixture.root)
        assert not fixture.provider.requests
        exited(terminal)
    (args.output / "summary.json").write_text(
        json.dumps({"full_command_intent_saved_before_effect": True}) + "\n"
    )


if __name__ == "__main__":
    main()
