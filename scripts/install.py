#!/usr/bin/env python3
"""Assemble an explicit local installation from already built artifacts."""
import argparse
import json
import pathlib
import shutil
import subprocess
import sys
from license_bundle import bundle

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = "eden-native-0.1.0"
ROLES = ["eden.agent-loop.v1", "eden.context.v1", "eden.provider.v1", "eden.tool.v1"]


def target():
    text = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
    return next(line.split(": ", 1)[1] for line in text.splitlines() if line.startswith("host:"))


def library(name):
    if sys.platform == "win32":
        return name + ".dll"
    return "lib" + name + (".dylib" if sys.platform == "darwin" else ".so")


def package(name, roles, path, triple, config=None):
    return {"descriptor": {"package": name, "version": "0.1.0", "provides": roles}, "host": CONTRACT, "sdk": CONTRACT, "target": triple, "library": path, "config": config}


def install(destination, profile="debug"):
    destination = pathlib.Path(destination).resolve()
    (destination / "bin").mkdir(parents=True, exist_ok=True)
    plugin_dir = destination / "plugins" / "standard" / "0.1.0"
    plugin_dir.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    shutil.copy2(ROOT / "target" / profile / ("eden" + suffix), destination / "bin" / ("eden" + suffix))
    name = library("eden_standard")
    shutil.copy2(ROOT / "target" / profile / name, plugin_dir / name)
    for doc in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"]:
        shutil.copy2(ROOT / doc, destination / doc)
    composition = {"packages": [package("standard", ROLES, f"plugins/standard/0.1.0/{name}", target())], "roles": {role: "standard" for role in ROLES}}
    (destination / "composition.json").write_text(json.dumps(composition, indent=2) + "\n", encoding="utf-8")
    bundle(ROOT, destination, target())
    return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=pathlib.Path)
    parser.add_argument("--profile", choices=["debug", "release"], default="debug")
    args = parser.parse_args()
    print(install(args.destination, args.profile))
