"""Exercise complete managed update transactions through installed CLI processes."""

import hashlib
import json
import os
import pathlib
import shutil
import subprocess
from typing import Any


def _write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def _manifest(directory: pathlib.Path, version: str) -> None:
    manifest = json.loads((directory / "release.json").read_text(encoding="utf-8"))
    manifest["version"] = version
    manifest["files"] = {
        path.relative_to(directory).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(directory.rglob("*"))
        if path.is_file() and path != directory / "release.json"
    }
    _write(directory / "release.json", manifest)


def verify_updates(installation: pathlib.Path, scratch: pathlib.Path) -> dict[str, Any]:
    """Use isolated copies and a local source; never contact or publish a real release."""
    installation = installation.resolve()
    work = scratch.resolve() / "updates"
    work.mkdir(parents=True, exist_ok=False)
    suffix = ".exe" if os.name == "nt" else ""
    eden = installation / "bin" / ("eden" + suffix)
    launcher = installation / "bin" / ("eden-launch" + suffix)
    managed = work / "managed"
    global_dir = work / "global"
    env = dict(os.environ, EDEN_MANAGED_ROOT=str(managed))
    evidence: list[dict[str, Any]] = []
    original_composition = (installation / "composition.json").read_bytes()
    original_manifest = (installation / "release.json").read_bytes()
    composition = json.loads(original_composition)
    for package in composition["packages"]:
        package["library"] = str(installation / package["library"])
        if package["descriptor"]["package"] == "distribution":
            package["config"] = {
                "root": str(global_dir / "distribution"),
                "updates": {
                    "sources": [
                        {
                            "target": {"kind": "host"},
                            "source": {"kind": "local", "path": str(work / "releases.json")},
                        }
                    ]
                },
            }
    control = work / "control.json"
    _write(control, composition)

    def run(arguments: list[str], *, success: bool = True) -> dict[str, Any]:
        result = subprocess.run(
            arguments,
            cwd=work,
            env=env,
            text=True,
            encoding="utf-8",
            capture_output=True,
            timeout=90,
            check=False,
        )
        evidence.append(
            {
                "command": arguments,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
        )
        assert (result.returncode == 0) == success, evidence[-1]
        return json.loads(result.stdout) if success else {"stderr": result.stderr}

    def update(request: dict[str, Any], *, success: bool = True) -> dict[str, Any]:
        return run(
            [
                str(eden),
                "--json",
                "--offline-startup",
                "--global-dir",
                str(global_dir),
                "--composition",
                str(control),
                "update",
                json.dumps(request),
            ],
            success=success,
        )

    def launch(expected: pathlib.Path) -> None:
        reply = run(
            [
                str(launcher),
                str(managed),
                "--json",
                "--offline-startup",
                "--global-dir",
                str(global_dir),
                "installation-check",
            ]
        )
        assert reply["installation"] == "ready", reply
        assert pathlib.Path(reply["path"]).resolve() == expected.resolve(), reply

    def check(channel: dict[str, str]) -> dict[str, Any]:
        reply = update({"operation": "check", "target": {"kind": "host"}, "channel": channel})
        assert reply["result"] == "checked", reply
        return reply["status"]["candidate"]

    def prepare(candidate: dict[str, Any], *, success: bool = True) -> dict[str, Any]:
        reply = update({"operation": "prepare", "candidate": candidate}, success=success)
        if not success:
            return reply
        assert reply["result"] == "prepared", reply
        return reply["prepared"]

    releases = []
    for version, prerelease in [("v0.1.0-update-a", False), ("v0.1.0-update-b", True)]:
        directory = work / version
        shutil.copytree(installation, directory)
        _manifest(directory, version)
        releases.insert(
            0,
            {
                "tag_name": version,
                "draft": False,
                "prerelease": prerelease,
                "source": {"kind": "local", "path": str(directory)},
            },
        )
    _write(work / "releases.json", releases)
    first_candidate = check({"kind": "stable"})
    second_candidate = check({"kind": "prerelease"})
    assert first_candidate["version"] == "v0.1.0-update-a"
    assert second_candidate["version"] == "v0.1.0-update-b"
    assert check({"kind": "tag", "tag": "v0.1.0-update-a"}) == {
        **first_candidate,
        "channel": {"kind": "tag", "tag": "v0.1.0-update-a"},
    }
    first = prepare(first_candidate)
    assert not (managed / "activations").exists(), "preparation activated a release"
    update({"operation": "activate", "prepared": first})
    first_path = pathlib.Path(first["path"])
    launch(first_path)

    # A checksum failure must leave the activated installation available.
    second_source = pathlib.Path(second_candidate["source"]["path"])
    package_bytes = (second_source / "package.json").read_bytes()
    (second_source / "package.json").write_text("changed", encoding="utf-8")
    failure = prepare(second_candidate, success=False)
    assert "UpdateIntegrity" in failure["stderr"], failure
    launch(first_path)
    (second_source / "package.json").write_bytes(package_bytes)

    # A valid inventory alone is insufficient: the complete new installation must start.
    worker = second_source / "bin" / ("eden-search-worker" + suffix)
    hidden_worker = work / ("withheld-worker" + suffix)
    worker.rename(hidden_worker)
    _manifest(second_source, second_candidate["version"])
    failure = prepare(second_candidate, success=False)
    assert "installation search worker missing" in failure["stderr"], failure
    launch(first_path)
    hidden_worker.rename(worker)
    _manifest(second_source, second_candidate["version"])

    second = prepare(second_candidate)
    launch(first_path)
    second_path = pathlib.Path(second["path"])
    receipt_bytes = (second_path / "package.json").read_bytes()
    (second_path / "package.json").write_text("changed after prepare", encoding="utf-8")
    failure = update({"operation": "activate", "prepared": second}, success=False)
    assert "UpdateIntegrity" in failure["stderr"], failure
    launch(first_path)
    (second_path / "package.json").write_bytes(receipt_bytes)
    update({"operation": "activate", "prepared": second})
    launch(second_path)
    assert (first_path / "composition.json").read_bytes() == original_composition
    assert (installation / "composition.json").read_bytes() == original_composition
    assert (installation / "release.json").read_bytes() == original_manifest
    run([str(first_path / "bin" / ("eden" + suffix)), "--json", "installation-check"])
    run([str(eden), "--json", "installation-check"])
    _write(work / "commands.json", evidence)
    return {
        "result": "passed",
        "first_installation": str(first_path),
        "second_installation": str(second_path),
        "original_installation_preserved": str(installation),
        "checks": [
            "stable/prerelease/exact-tag discovery",
            "prepare without activation",
            "complete installed startup and launcher selection",
            "checksum failure preserves active release",
            "startup failure preserves active release",
            "changed prepared bytes cannot activate",
            "old and original installations remain runnable",
        ],
        "commands": str(work / "commands.json"),
        "real_release_access": False,
    }
