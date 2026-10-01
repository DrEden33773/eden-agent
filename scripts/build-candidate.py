#!/usr/bin/env python3
"""Build and install one coherent local candidate, with source and artifact receipts."""

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

from frontend_tools import protoc
from install import ROOT, build_target, install
from verification import source_fingerprint


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    before = source_fingerprint()
    profile = "release" if args.release else "debug"
    command = ["cargo", "build", "--workspace", "--locked"]
    if args.release:
        command.append("--release")
    subprocess.run(command, cwd=ROOT, check=True)
    subprocess.run(
        [
            sys.executable,
            ROOT / "scripts/build-frontend.py",
            *(["--release"] if args.release else []),
        ],
        cwd=ROOT,
        check=True,
    )
    assert before == source_fingerprint(), "source changed during the candidate build"
    destination = install(args.destination, profile)
    installed = json.loads((destination / "release.json").read_text())
    frontend_target = Path(
        os.environ.get("EDEN_FRONTEND_TARGET_DIR", str(ROOT / "target/frontend"))
    )
    origins = {}
    for relative, digest in installed["files"].items():
        path = Path(relative)
        if path.parts[0] not in {"bin", "plugins", "ui"}:
            continue
        original = (
            (frontend_target / profile / ("xai-grok-pager" + (".exe" if os.name == "nt" else "")))
            if path.name.startswith("eden-frontend")
            else build_target() / profile / path.name
        )
        actual = hashlib.sha256(original.read_bytes()).hexdigest()
        assert actual == digest, f"installed artifact differs from its build output: {relative}"
        origins[relative] = {"build_output": str(original), "sha256": actual}
    receipt = {
        "source_fingerprint": before,
        "commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "frontend_reference": (ROOT / "frontend/grok/SOURCE_REV").read_text().strip(),
        "builds": {"root": "passed", "frontend": "passed"},
        "tools": {
            "rust": subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True).strip(),
            "protoc": subprocess.check_output([protoc(), "--version"], cwd=ROOT, text=True).strip(),
        },
        "origins": origins,
        "installed_files": installed["files"],
    }
    (destination / "CANDIDATE.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(destination)


if __name__ == "__main__":
    main()
