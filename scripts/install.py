#!/usr/bin/env python3
"""Assemble an explicit local installation from already built artifacts."""

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys
from functools import cache
from typing import Any, NotRequired, TypedDict

from license_bundle import bundle


# Composition is authored here; arbitrary plugin config remains a JSON boundary.
class Descriptor(TypedDict):
    package: str
    version: str
    provides: list[str]


class Package(TypedDict):
    requires: NotRequired[list[str]]
    descriptor: Descriptor
    host: str
    sdk: str
    target: str
    library: str
    config: dict[str, Any] | None


class Composition(TypedDict):
    packages: list[Package]
    roles: dict[str, str]


ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = "eden-native-0.4.0"
ROLES = ["eden.agent-loop.v1", "eden.context.v1", "eden.provider.v1", "eden.tool.v1"]


@cache
def target() -> str:
    text = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
    return next(line.split(": ", 1)[1] for line in text.splitlines() if line.startswith("host:"))


def build_target() -> pathlib.Path:
    path = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    return (ROOT / path).resolve()


def library(name: str) -> str:
    if sys.platform == "win32":
        return name + ".dll"
    return "lib" + name + (".dylib" if sys.platform == "darwin" else ".so")


def package(
    name: str,
    roles: list[str],
    path: str,
    triple: str,
    config: dict[str, Any] | None = None,
) -> Package:
    return {
        "descriptor": {"package": name, "version": "0.1.0", "provides": roles},
        "host": CONTRACT,
        "sdk": CONTRACT,
        "target": triple,
        "library": path,
        "config": config,
    }


def install(
    destination: pathlib.Path | str,
    profile: str = "debug",
    controlled: bool = False,
    *,
    license_directory: pathlib.Path | None = None,
) -> pathlib.Path:
    destination = pathlib.Path(destination).resolve()
    (destination / "bin").mkdir(parents=True, exist_ok=True)
    plugin_dir = destination / "plugins" / "standard" / "0.1.0"
    plugin_dir.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    shutil.copy2(
        build_target() / profile / ("eden" + suffix),
        destination / "bin" / ("eden" + suffix),
    )
    name = library("eden_standard")
    if controlled:
        shutil.copy2(build_target() / profile / name, plugin_dir / name)
    for doc in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"]:
        shutil.copy2(ROOT / doc, destination / doc)
    composition: Composition = {
        "packages": [package("standard", ROLES, f"plugins/standard/0.1.0/{name}", target())],
        "roles": {role: "standard" for role in ROLES},
    }
    if not controlled:
        defaults: list[tuple[str, list[str]]] = [
            (
                "coding",
                [
                    "eden.coding-loop.v2",
                    "eden.coding-context.v2",
                    "eden.submission-queue.v2",
                ],
            ),
            ("coding-tools", ["eden.tool-catalog.v1", "eden.coding-tool.v1"]),
            (
                "model-access",
                [
                    "eden.model-catalog.v1",
                    "eden.model-manager.v1",
                    "eden.credential-source.v1",
                    "eden.auth.v1",
                    "eden.model-info.v1",
                    "eden.coding-provider.v1",
                ],
            ),
            ("local-history", ["eden.session-store.v2"]),
            ("workspace-resources", ["eden.resource-source.v1"]),
            ("distribution", ["eden.distribution-commands.v1", "eden.distribution.v1"]),
            (
                "contributions",
                [
                    "eden.command-catalog.v1",
                    "eden.command.v1",
                    "eden.before-input.v1",
                    "eden.before-tool.v1",
                ],
            ),
            (
                "search",
                [
                    "eden.search-catalog.v1",
                    "eden.search-tool.v1",
                    "eden.search-access.v1",
                    "eden.instance-stop.v1",
                ],
            ),
        ]
        composition = {"packages": [], "roles": {}}
        for pkg, roles in defaults:
            lib = library("eden_" + pkg.replace("-", "_"))
            relative = f"plugins/{pkg}/0.1.0/{lib}"
            (destination / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(build_target() / profile / lib, destination / relative)
            config = None
            if pkg == "coding-tools":
                config = {
                    "read_observer": "eden.search-access.v1",
                    "tools": ["read", "write", "edit", "bash", "skill"],
                    "contributions": [
                        {
                            "catalog": "eden.search-catalog.v1",
                            "execute": "eden.search-tool.v1",
                            "read_only": True,
                        }
                    ],
                }
            if pkg == "contributions":
                config = {
                    "commands": [
                        {
                            "catalog": "eden.distribution-commands.v1",
                            "execute": "eden.distribution.v1",
                        }
                    ]
                }
            composition["packages"].append(package(pkg, roles, relative, target(), config))
            composition["roles"].update(
                {role: pkg for role in roles if role != "eden.instance-stop.v1"}
            )
    if not controlled:
        shutil.copy2(
            build_target() / profile / ("eden-search-worker" + suffix),
            destination / "bin" / ("eden-search-worker" + suffix),
        )
    (destination / "composition.json").write_text(
        json.dumps(composition, indent=2) + "\n", encoding="utf-8"
    )
    if license_directory is None:
        bundle(ROOT, destination, target())
    else:
        shutil.copytree(license_directory, destination / "third-party-licenses", dirs_exist_ok=True)
    return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=pathlib.Path)
    parser.add_argument("--profile", choices=["debug", "release"], default="debug")
    args = parser.parse_args()
    print(install(args.destination, args.profile))
