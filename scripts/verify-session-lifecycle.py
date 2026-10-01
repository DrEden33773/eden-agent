#!/usr/bin/env python3
"""Run the installed frontend lifecycle regressions from one frozen host seed."""

import json
import platform
import sys
from pathlib import Path

from install import ROOT
from verification import prepare, run


def main():
    receipt = ROOT / "artifacts/session-lifecycle-verification.json"
    if sys.platform != "linux":
        receipt.write_text(
            json.dumps(
                {
                    "platform": platform.platform(),
                    "pty": "not exercised; Linux process and PTY evidence is selected separately",
                },
                indent=2,
            )
            + "\n"
        )
        return
    installation = Path(prepare()["seeds"]) / "default"
    results = {}
    for name in ("recovery", "boundaries", "execution"):
        output = ROOT / "artifacts/session-lifecycle" / name
        run(
            [
                sys.executable,
                ROOT / f"scripts/verify-session-{name}.py",
                "--installation",
                installation,
                "--output",
                output,
            ],
            ROOT,
            timeout=None,
        )
        results[name] = json.loads((output / "summary.json").read_text())
    receipt.write_text(
        json.dumps(
            {
                "platform": platform.platform(),
                "installed_seed": str(installation),
                "results": results,
            },
            indent=2,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
