#!/usr/bin/env python3
"""Compatibility build command: the terminal is part of the installed Eden binary."""

import argparse
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    command = ["cargo", "build", "--locked", "-p", "eden-cli"]
    if args.release:
        command.append("--release")
    subprocess.run(command, cwd=ROOT, check=True)


if __name__ == "__main__":
    main()
