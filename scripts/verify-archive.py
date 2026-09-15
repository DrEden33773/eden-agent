#!/usr/bin/env python3
"""Verify and execute the packaged installation on its native CI runner."""

import argparse
import hashlib
import json
import os
import pathlib
import platform
import subprocess
import tarfile
import tempfile
from typing import Any, TypedDict

from install import ROOT, target
from verification import EXAMPLES, digest, run, source_fingerprint, validate_receipt


class FileState(TypedDict):
    sha256: str
    executable_bits: int


def file_state(root: pathlib.Path) -> dict[str, FileState]:
    result: dict[str, FileState] = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"Unexpected installation symlink: {path}")
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {
                "sha256": digest(path),
                "executable_bits": path.stat().st_mode & 0o111 if os.name != "nt" else 0,
            }
    return result


def compare_files(actual: dict[str, FileState], expected: dict[str, FileState]) -> None:
    missing = sorted(expected.keys() - actual.keys())
    extra = sorted(actual.keys() - expected.keys())
    if missing or extra:
        raise ValueError(f"Archive file set differs: missing={missing}, extra={extra}")
    changed = sorted(name for name in actual if actual[name] != expected[name])
    if changed:
        raise ValueError(f"Archive bytes or executable permissions differ: {changed}")


def extract_verified(
    archive: pathlib.Path, destination: pathlib.Path, expected: dict[str, FileState]
) -> None:
    """Extract only ordinary relative files/directories, then verify every file."""
    with tarfile.open(archive, "r:gz") as bundle:
        seen = set()
        for member in bundle.getmembers():
            path = pathlib.PurePosixPath(member.name)
            if (
                path.is_absolute()
                or ".." in path.parts
                or pathlib.PureWindowsPath(member.name).drive
                or "\\" in member.name
                or not (member.isfile() or member.isdir())
            ):
                raise ValueError(f"Unsafe archive entry: {member.name}")
            if member.isfile():
                name = path.as_posix()
                if name in seen:
                    raise ValueError(f"Duplicate archive file: {name}")
                seen.add(name)
                if os.name != "nt" and name in expected:
                    if member.mode & 0o111 != expected[name]["executable_bits"]:
                        raise ValueError(f"Archive executable permissions differ: {name}")
        if seen != expected.keys():
            raise ValueError(
                f"Archive member set differs: missing={sorted(expected.keys() - seen)}, "
                f"extra={sorted(seen - expected.keys())}"
            )
        bundle.extractall(destination, filter="data")
    compare_files(file_state(destination), expected)


def verify_archive(
    archive: pathlib.Path, distribution: pathlib.Path, prepared_path: pathlib.Path
) -> dict[str, Any]:
    prepared = json.loads(prepared_path.read_text(encoding="utf-8"))
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    if prepared["commit"] != commit:
        raise ValueError("Packaged verification must use the prepared source commit")
    validate_receipt(prepared, source_fingerprint())
    suffix = ".exe" if os.name == "nt" else ""
    seed = pathlib.Path(prepared["seeds"]) / "default"
    expected = file_state(seed)
    for example in EXAMPLES:
        del expected[f"bin/{example}{suffix}"]
    compare_files(file_state(distribution), expected)
    native_target = target()
    with tempfile.TemporaryDirectory(prefix="eden-archive-验收-") as temp:
        root = pathlib.Path(temp)
        parent_receipt = root / "receipt.json"
        parent_bytes = b'{"purpose":"unrelated download receipt; not a managed package"}\n'
        parent_receipt.write_bytes(parent_bytes)
        installed = root / "installation"
        installed.mkdir()
        extract_verified(archive, installed, expected)
        composition = json.loads((installed / "composition.json").read_text(encoding="utf-8"))
        if not composition["packages"] or any(
            package["target"] != native_target for package in composition["packages"]
        ):
            raise ValueError("Archive package targets differ from the native runner")
        caller = root / "unrelated-caller"
        project = root / "isolated-project"
        global_dir = root / "isolated-global"
        for directory in (caller, project, global_dir):
            directory.mkdir()
        marker = "EDEN-PACKAGED-NATIVE-RESOURCE"
        (project / "AGENTS.md").write_text(marker + "\n", encoding="utf-8")
        host = installed / "bin" / ("eden" + suffix)
        version = run([host, "--version"], caller).stdout.strip()
        if not version.startswith("eden "):
            raise ValueError(f"Unexpected packaged CLI version: {version}")
        resource_result = run(
            [host, "resources", "list", "--cwd", project, "--global-dir", global_dir], caller
        )
        resources = json.loads(resource_result.stdout)
        if marker not in resources["instructions"]:
            raise ValueError("Packaged CLI did not read the isolated project resources")
        if parent_receipt.read_bytes() != parent_bytes:
            raise ValueError("Packaged CLI changed the unrelated parent receipt")
        compare_files(file_state(installed), expected)
    # The source seed and its independent-author proof must remain fixed after the probes too.
    validate_receipt(prepared, source_fingerprint())
    return {
        "status": "passed",
        "commit": commit,
        "source_dirty": prepared["dirty"],
        "source_fingerprint": prepared["source_fingerprint"],
        "target": native_target,
        "platform": platform.platform(),
        "archive": archive.name,
        "archive_sha256": digest(archive),
        "file_count": len(expected),
        "files_sha256": hashlib.sha256(json.dumps(expected, sort_keys=True).encode()).hexdigest(),
        "host_sha256": expected[f"bin/eden{suffix}"]["sha256"],
        "library_sha256": {
            package["library"]: expected[package["library"]]["sha256"]
            for package in composition["packages"]
        },
        "file_bytes_match_frozen_distribution": True,
        "executable_permissions": (
            "native Windows execution verified; POSIX mode bits do not describe Windows ACLs"
            if os.name == "nt"
            else "archive and extracted executable bits match the frozen distribution"
        ),
        "packaged_cli_version": version,
        "packaged_resources_marker": marker,
        "isolated_cwd_and_global_dir": True,
        "unrelated_parent_receipt_unchanged": True,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument(
        "--distribution", type=pathlib.Path, default=ROOT / "artifacts/distribution"
    )
    parser.add_argument(
        "--prepared", type=pathlib.Path, default=ROOT / "artifacts/verification/prepared.json"
    )
    parser.add_argument(
        "--output", type=pathlib.Path, default=ROOT / "artifacts/ci/archive-verification.json"
    )
    args = parser.parse_args()
    report: dict[str, Any] = {"status": "failed", "archive": str(args.archive)}
    try:
        report = verify_archive(args.archive.resolve(), args.distribution.resolve(), args.prepared)
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
