#!/usr/bin/env python3
"""Capture startup and replay frames and reject inherited Grok branding."""

import argparse
import json
from pathlib import Path

from verify_workflows import ROOT, Fixture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/s4-g2-host")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output, configuration_author=False) as fixture:
        assert fixture.endpoint is not None
        fixture.terminal_start()
        assert fixture.terminal is not None
        fixture.capture("startup")
        raw = bytes(fixture.terminal.output)
        assert b"Grok Build" not in raw, "startup painted the inherited Grok hero"
        assert b"]0;grok" not in raw, "terminal title retains upstream branding"
        fixture.stop_terminal()
        assert b"grok --resume" not in fixture.terminal.output, (
            "exit advertises the wrong executable"
        )
        fixture.terminal_start()
        assert fixture.terminal is not None
        fixture.capture("reattach")
        assert b"Grok Build" not in fixture.terminal.output
        fixture.stop_terminal()
        # The launcher is still usable when the selected endpoint cannot connect.
        endpoint = json.loads(fixture.endpoint_file.read_text())
        endpoint["address"] = "127.0.0.1:1"
        fixture.endpoint_file.write_text(json.dumps(endpoint))
        from verify import HERE, Terminal

        fixture.terminal = Terminal(HERE / "run.py", fixture.endpoint_file, width=110, height=40)
        fixture.terminal.wait("Eden connection failed", seconds=20)
        fixture.capture("failed")
        assert b"Grok Build" not in fixture.terminal.output
        fixture.terminal.close(expected_code=None, screen=False)
        for failure in ("token", "session_id"):
            endpoint = dict(fixture.endpoint)
            endpoint[failure] = "invalid-token" if failure == "token" else endpoint[failure] + 1
            fixture.endpoint_file.write_text(json.dumps(endpoint))
            fixture.terminal = Terminal(
                HERE / "run.py", fixture.endpoint_file, width=110, height=40
            )
            fixture.terminal.wait("Eden connection failed", seconds=10)
            fixture.capture(f"failed-{failure}")
            assert b"Grok Build" not in fixture.terminal.output
            fixture.terminal.close(expected_code=None, screen=False)
    (args.output / "summary.json").write_text(
        json.dumps({"startup": True, "reattach": True, "failure_exit": True}) + "\n"
    )


if __name__ == "__main__":
    main()
