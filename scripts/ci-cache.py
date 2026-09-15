"""Derive native build cache keys from the build environment, never CI verdicts."""

import hashlib
import json
import os
import pathlib
import re
import subprocess
import tomllib
from collections.abc import Mapping
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[1]
LEGACY_SOURCE_COMMIT = "51dffd733890e2cb15b29588cfbb03c0e6bdbeb2"
LEGACY_CACHE_COMMIT = "99564f5b129d0e393c4565ba4f666210efe078d4"
LEGACY_BUILD_INPUTS = "294cf63576952c6443b66321011a0c8ad772e140882ab1651669e9946b9ffa75"
# Recorded from the completed native jobs in run 34955722547. Windows checkout
# line endings gave its old hashFiles key a different digest from Unix runners.
LEGACY_IMAGES = {
    ("ubuntu-24.04", "X64"): (
        "20260907.300.1",
        "5b3a04204a6d968eee4150c199d915af03817fef3216954ca9e46b88a4e10851",
    ),
    ("macos-14", "ARM64"): (
        "20260831.0302.1",
        "5b3a04204a6d968eee4150c199d915af03817fef3216954ca9e46b88a4e10851",
    ),
    ("windows-2022", "X64"): (
        "20260907.297.1",
        "4b0b0b6b569a46fa9c2440c83860c9c74140b816c997d40566b6998c9874fab5",
    ),
}
BUILD_ENV = {
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_TARGET",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
}


def build_environment(environment: Mapping[str, str]) -> dict[str, str]:
    return {
        key: value
        for key, value in environment.items()
        if key in BUILD_ENV or key.startswith(("CARGO_PROFILE_", "CARGO_TARGET_"))
    }


def digest(value: Any) -> str:
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def is_build_input(name: str) -> bool:
    path = pathlib.PurePosixPath(name)
    return path.name in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"} or (
        path.parent.name == ".cargo" and path.name in {"config", "config.toml"}
    )


def read_inputs(root: pathlib.Path) -> dict[str, str]:
    names = (
        subprocess.check_output(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root
        )
        .decode()
        .split("\0")
    )
    # TOML text is semantically identical under checkout CRLF conversion. Source
    # files/mtimes are not modified, restored or used as cached verification proof.
    return {
        name: (root / name).read_text(encoding="utf-8")
        for name in sorted(set(names))
        if is_build_input(name) and (root / name).is_file()
    }


def identities(files: Mapping[str, str], environment: Mapping[str, str]) -> tuple[str, str]:
    normalized = {name: text.replace("\r\n", "\n") for name, text in files.items()}
    if "Cargo.toml" not in normalized or "rust-toolchain.toml" not in normalized:
        raise ValueError("cache identity needs root Cargo.toml and rust-toolchain.toml")
    configs = {
        name: text
        for name, text in normalized.items()
        if pathlib.PurePosixPath(name).name == "rust-toolchain.toml"
        or pathlib.PurePosixPath(name).parent.name == ".cargo"
    }
    profiles = {
        name: profile
        for name, text in normalized.items()
        if pathlib.PurePosixPath(name).name == "Cargo.toml"
        and (profile := tomllib.loads(text).get("profile"))
    }
    build_env = build_environment(environment)
    context = digest({"configs": configs, "profiles": profiles, "environment": build_env})
    inputs = digest({"files": normalized, "environment": build_env})
    return context, inputs


def cache_keys(
    files: Mapping[str, str],
    environment: Mapping[str, str],
    runner_os: str,
    architecture: str,
    image: str,
    commit: str,
) -> dict[str, Any]:
    for value in (runner_os, architecture, image):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", value):
            raise ValueError("cache context requires explicit OS, architecture and image version")
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise ValueError("cache key requires an exact commit ID")
    environment_hash, inputs = identities(files, environment)
    context = digest({"environment": environment_hash, "image": image})
    prefix = f"cargo-target-v2-{runner_os}-{architecture}-{context}-"
    restore = [f"{prefix}{inputs}-", prefix]
    legacy = LEGACY_IMAGES.get((runner_os, architecture))
    legacy_key = None
    if legacy and legacy[0] == image and inputs == LEGACY_BUILD_INPUTS:
        legacy_key = f"cargo-target-v1-{runner_os}-{architecture}-{legacy[1]}-{LEGACY_CACHE_COMMIT}"
        restore.append(legacy_key)
    return {
        "key": f"{prefix}{inputs}-{commit}",
        "restore_keys": restore,
        "context": context,
        "build_inputs": inputs,
        "os": runner_os,
        "architecture": architecture,
        "image_version": image,
        "build_environment": build_environment(environment),
        "input_files": sorted(files),
        "legacy_candidate": legacy_key,
        "legacy_source_commit": LEGACY_SOURCE_COMMIT if legacy_key else None,
        "legacy_run_id": "34955722547" if legacy_key else None,
    }


def main() -> None:
    result = cache_keys(
        read_inputs(ROOT),
        os.environ,
        os.environ.get("CI_CACHE_OS", ""),
        os.environ.get("CI_CACHE_ARCH", ""),
        os.environ.get("ImageVersion", ""),
        os.environ.get("GITHUB_SHA", ""),
    )
    output = ROOT / "artifacts/ci"
    output.mkdir(parents=True, exist_ok=True)
    (output / "cache-inputs.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    if destination := os.environ.get("GITHUB_OUTPUT"):
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(f"key={result['key']}\nrestore-keys<<EDEN_CACHE_KEYS\n")
            stream.write("\n".join(result["restore_keys"]) + "\nEDEN_CACHE_KEYS\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
