#!/usr/bin/env python3
"""Build and install one coherent local candidate, with source and artifact receipts."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

from install import ROOT, build_target, install
from verification import HOST_TARGETS, source_fingerprint


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--release", action="store_true")
    parser.add_argument(
        "--cargo-config",
        action="append",
        default=[],
        help="Cargo profile override, recorded in the candidate receipt",
    )
    args = parser.parse_args()
    before = source_fingerprint()
    profile = "release" if args.release else "debug"
    command = [
        "cargo",
        *[argument for config in args.cargo_config for argument in ("--config", config)],
        "build",
        *(["--workspace"] if args.release else HOST_TARGETS),
        "--locked",
    ]
    if args.release:
        command.append("--release")
    subprocess.run(command, cwd=ROOT, check=True)
    assert before == source_fingerprint(), "source changed during the candidate build"
    destination = install(args.destination, profile)
    installed = json.loads((destination / "release.json").read_text())
    origins = {}
    for relative, digest in installed["files"].items():
        path = Path(relative)
        if path.parts[0] not in {"bin", "plugins", "ui"}:
            continue
        original = build_target() / profile / path.name
        actual = hashlib.sha256(original.read_bytes()).hexdigest()
        assert actual == digest, f"installed artifact differs from its build output: {relative}"
        origins[relative] = {"build_output": str(original), "sha256": actual}
    receipt = {
        "source_fingerprint": before,
        "commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "frontend_reference": (ROOT / "frontend/grok/UPSTREAM_COMMIT").read_text().strip(),
        "frontend_source_rev": (ROOT / "frontend/grok/SOURCE_REV").read_text().strip(),
        "builds": {"root_workspace_with_terminal": "passed"},
        "build_command": command,
        "tools": {
            "rust": subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True).strip(),
        },
        "origins": origins,
        "installed_files": installed["files"],
    }
    (destination / "CANDIDATE.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(destination)


if __name__ == "__main__":
    main()
