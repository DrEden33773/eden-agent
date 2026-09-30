#!/usr/bin/env python3
"""Build the fixed Grok-derived pager and the Eden adapter into a local candidate."""

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SUFFIX = ".exe" if os.name == "nt" else ""


def copy(source, destination):
    temporary = destination.with_name(destination.name + ".next")
    shutil.copy2(source, temporary)
    temporary.replace(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT / "artifacts/g1-grok-source")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/g1-native-port")
    parser.add_argument("--protoc", type=Path)
    args = parser.parse_args()
    source = args.source.resolve()
    output = args.output.resolve()
    environment = dict(os.environ)
    protoc = args.protoc or environment.get("PROTOC") or shutil.which("protoc")
    local = ROOT / "artifacts/grok-build-tools/bin/protoc"
    if not protoc and local.exists():
        protoc = local
    if not protoc:
        parser.error("protoc is required; pass --protoc /path/to/protoc")
    environment["PROTOC"] = str(Path(protoc).resolve())
    environment["CARGO_TARGET_DIR"] = str(ROOT / "artifacts/g1-native-target")
    environment.setdefault("CARGO_BUILD_JOBS", "4")
    subprocess.run([sys.executable, str(HERE / "prepare.py"), str(source)], check=True)
    subprocess.run(
        [
            "cargo",
            "+1.98.1",
            "build",
            "--manifest-path",
            str(source / "Cargo.toml"),
            "--locked",
            "-p",
            "xai-grok-pager-bin",
            "--bin",
            "xai-grok-pager",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )
    subprocess.run(["cargo", "build", "--locked", "-p", "eden-grok-adapter"], cwd=ROOT, check=True)
    output.mkdir(parents=True, exist_ok=True)
    copy(
        ROOT / f"artifacts/g1-native-target/debug/xai-grok-pager{SUFFIX}",
        output / f"eden-grok{SUFFIX}",
    )
    copy(ROOT / f"target/debug/eden-grok-adapter{SUFFIX}", output / f"eden-grok-adapter{SUFFIX}")
    for name in ["LICENSE", "THIRD-PARTY-NOTICES"]:
        copy(source / name, output / name)
    copy(HERE / "README.md", output / "EDEN-FRONTEND.md")
    copy(ROOT / "NOTICE", output / "EDEN-NOTICE")
    copy(ROOT / "THIRD_PARTY_NOTICES.md", output / "EDEN-THIRD-PARTY-NOTICES.md")
    print(f"Candidate: {output}")


if __name__ == "__main__":
    main()
