#!/usr/bin/env python3
"""Build the checked-in terminal source; runtime never prepares or builds source."""

import argparse
import os
import subprocess
from pathlib import Path

from frontend_tools import protoc

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    command = [
        "cargo",
        "build",
        "--manifest-path",
        str(ROOT / "frontend/grok/Cargo.toml"),
        "--locked",
        "-p",
        "xai-grok-pager-bin",
    ]
    if args.release:
        command.append("--release")
    subprocess.run(
        command,
        cwd=ROOT,
        env={
            **os.environ,
            "PROTOC": str(protoc()),
            "CARGO_TARGET_DIR": os.environ.get(
                "EDEN_FRONTEND_TARGET_DIR", str(ROOT / "target/frontend")
            ),
        },
        check=True,
    )


if __name__ == "__main__":
    main()
