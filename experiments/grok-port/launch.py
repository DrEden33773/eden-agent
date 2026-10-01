#!/usr/bin/env python3
"""Compatibility helper delegating every lifecycle choice to the installed Eden CLI."""

import argparse
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", choices=["live-tui"])
    parser.add_argument(
        "--host", type=Path, default=ROOT / "artifacts/session-recovery-candidate/bin/eden"
    )
    parser.add_argument("--cwd", type=Path)
    parser.add_argument("--endpoint", type=Path)
    parser.add_argument("--resume", "--history", dest="history", type=Path)
    args = parser.parse_args()
    command = [str(args.host.resolve())]
    if args.cwd:
        command.extend(["--cwd", str(args.cwd)])
    if args.history:
        command.extend(["--session", str(args.history)])
    if args.endpoint:
        command.extend(["tui", "--endpoint", str(args.endpoint)])
    os.execv(command[0], command)


if __name__ == "__main__":
    main()
