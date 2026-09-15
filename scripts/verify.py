#!/usr/bin/env python3
"""Build host first, then independent SDK consumers; exercise installed artifacts."""

import copy
import hashlib
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
from typing import Any

from install import ROLES, ROOT, library, package, target
from verification import author_artifact, installed, prepare, run


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    prepare()
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = installed(artifacts / "install", controlled=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    fixed = {
        path.name: path.read_bytes() for path in (destination / "bin").iterdir() if path.is_file()
    }
    host = destination / "bin" / ("eden" + suffix)
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    triple = target()
    with tempfile.TemporaryDirectory(prefix="eden-independent-authors-") as temp:
        scratch = pathlib.Path(temp)
        for name, roles in [
            ("loop-a", [ROLES[0]]),
            ("context-b", [ROLES[1]]),
            ("lifecycle", [ROLES[0]]),
            ("init-probe", [ROLES[0]]),
        ]:
            lib = library("author_" + name.replace("-", "_"))
            plugin_dir = destination / "plugins" / name / "0.1.0"
            plugin_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy2(author_artifact(name), plugin_dir / lib)
            composition["packages"].append(
                package(name, roles, f"plugins/{name}/0.1.0/{lib}", triple)
            )
        invalid_paths = {}
        for feature in [
            "wrong-abi",
            "wrong-sdk",
            "short-table",
            "metadata-panic",
            "init-panic",
        ]:
            lib = library("author_invalid")
            invalid_dir = destination / "plugins" / feature
            invalid_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy2(author_artifact("invalid", feature), invalid_dir / lib)
            invalid_paths[feature] = f"plugins/{feature}/{lib}"
        caller = scratch / "unrelated caller 工作目录"
        caller.mkdir()
        base = copy.deepcopy(composition)
        # Only explicitly used packages are enabled; lifecycle requires its test-owned endpoint.
        base["packages"] = [
            p
            for p in base["packages"]
            if p["descriptor"]["package"] not in ["lifecycle", "init-probe"]
        ]
        for name, loop, context, expected, order in [
            (
                "default",
                "standard",
                "standard",
                "standard:hello => echo[standard:hello]",
                [ROLES[0], ROLES[1], ROLES[2], ROLES[3], ROLES[2]],
            ),
            (
                "loop_a",
                "loop-a",
                "standard",
                "A/standard:hello => echo[A:standard:hello]",
                [ROLES[0], ROLES[1], ROLES[3], ROLES[2]],
            ),
            (
                "context_b",
                "standard",
                "context-b",
                "B<HELLO> => echo[B<HELLO>]",
                [ROLES[0], ROLES[1], ROLES[2], ROLES[3], ROLES[2]],
            ),
            (
                "mixed",
                "loop-a",
                "context-b",
                "A/B<HELLO> => echo[A:B<HELLO>]",
                [ROLES[0], ROLES[1], ROLES[3], ROLES[2]],
            ),
        ]:
            selected = copy.deepcopy(base)
            selected["roles"][ROLES[0]] = loop
            selected["roles"][ROLES[1]] = context
            path = destination / f"{name}.json"
            write(path, selected)
            events = [
                json.loads(line)
                for line in run(
                    [host, "--composition", path, "--json", "hello"], caller
                ).stdout.splitlines()
            ]
            assert events[0]["kind"] == "accepted" and events[-1]["kind"] == "settled"
            assert events[-1]["payload"] == {
                "outcome": {"status": "completed", "value": expected},
                "cleanup_errors": [],
            }
            assert [
                e["payload"]["contract"] for e in events if e["kind"] == "service_called"
            ] == order
            assert [e["sequence"] for e in events] == list(range(1, len(events) + 1))
            assert len({(e["session_id"], e["run_id"]) for e in events}) == 1
            model_inputs = [e["payload"] for e in events if e["kind"] == "model_input"]
            assert model_inputs[-1]["tool_result"] in expected
            assert (
                run([host, "--composition", path, "--print", "hello"], destination).stdout.strip()
                == expected
            )
            results[name] = {
                "result": expected,
                "order": order,
                "model_inputs": model_inputs,
            }
        assert (
            run([host, "hello"], caller).stdout.strip() == "standard:hello => echo[standard:hello]"
        )
        embedded = json.loads(
            run(
                [
                    destination / "bin" / ("embedded" + suffix),
                    destination / "composition.json",
                ],
                caller,
            ).stdout
        )
        assert embedded["outcome"]["value"] == results["default"]["result"]
        results["embedded"] = embedded
        for feature, lib in invalid_paths.items():
            selected = copy.deepcopy(base)
            selected["packages"].append(package("invalid", [ROLES[0]], lib, triple))
            path = destination / f"{feature}.json"
            write(path, selected)
            result = run([host, "--composition", path, "hello"], caller, check=False)
            expected = "PluginFailure" if "panic" in feature else "IncompatibleContract"
            assert result.returncode != 0 and expected in result.stderr, (
                feature,
                result.stderr,
            )
            results[feature] = expected
        initialized = copy.deepcopy(base)
        probe_package = next(
            p for p in composition["packages"] if p["descriptor"]["package"] == "init-probe"
        )
        initialized["packages"].append(copy.deepcopy(probe_package))
        initialized["roles"][ROLES[0]] = "init-probe"
        path = destination / "init-probe.json"
        write(path, initialized)
        results["abandoned_init"] = json.loads(
            run([destination / "bin" / ("initialization_probe" + suffix), path], caller).stdout
        )
        marker = scratch / "destroy-marker"
        initialized["packages"][-1]["config"] = {"marker": str(marker)}
        write(path, initialized)
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run(
                [str(host), "--composition", str(path), "--print", "hello"],
                cwd=caller,
                stdout=writer,
                stderr=subprocess.PIPE,
                timeout=20,
            )
        finally:
            os.close(writer)
        assert result.returncode != 0 and marker.read_text() == "destroyed"
        results["broken_stdout"] = "error returned after native instance destruction"
        rollback_marker = scratch / "rollback-marker"
        initialized["packages"][-1]["config"] = {"marker": str(rollback_marker)}
        initialized["packages"].append(
            package("invalid", [ROLES[0]], invalid_paths["init-panic"], triple)
        )
        write(path, initialized)
        result = run([host, "--composition", path, "hello"], caller, check=False)
        assert result.returncode != 0 and "PluginFailure" in result.stderr
        assert rollback_marker.read_text() == "destroyed"
        results["partial_initialization_rollback"] = "previous native instance destroyed"
        for mode in [
            "complete",
            "cancel",
            "dropped_receiver",
            "cleanup_error",
            "failed_then_cancel",
            "panic",
            "drop_panic",
        ]:
            selected = copy.deepcopy(composition)
            selected["packages"] = [
                p for p in selected["packages"] if p["descriptor"]["package"] != "init-probe"
            ]
            selected["roles"][ROLES[0]] = "lifecycle"
            path = destination / "lifecycle.json"
            write(path, selected)
            result = run([destination / "bin" / ("contract_probe" + suffix), path, mode], caller)
            results[mode] = json.loads(result.stdout)
        for field, value, expected_code in [
            ("sdk", "wrong", "IncompatibleContract"),
            ("host", "wrong", "IncompatibleContract"),
            ("target", "wrong", "IncompatibleContract"),
            ("library", "absent.so", "MissingDependency"),
            ("config", {"fail_init": True}, "Unavailable"),
        ]:
            selected = copy.deepcopy(base)
            selected["packages"][0][field] = value
            path = destination / f"invalid-{field}.json"
            write(path, selected)
            result = run([host, "--composition", path, "hello"], caller, check=False)
            assert result.returncode != 0 and expected_code in result.stderr, (
                field,
                result.stderr,
            )
            results[f"reject_{field}"] = expected_code
        selected = copy.deepcopy(base)
        del selected["roles"][ROLES[1]]
        path = destination / "missing-role.json"
        write(path, selected)
        result = run([host, "--composition", path, "hello"], caller, check=False)
        assert result.returncode != 0 and "MissingDependency" in result.stderr
        results["missing_role"] = "MissingDependency"
        assert all(
            (destination / "bin" / name).read_bytes() == data for name, data in fixed.items()
        )
    evidence = {
        "platform": platform.platform(),
        "target": triple,
        "rust": run(["rustc", "--version"], ROOT).stdout.strip(),
        "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(),
        "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()),
        "host_unchanged": True,
        "host_sha256": hashlib.sha256(fixed[host.name]).hexdigest(),
        "independent_author_build": True,
        "results": results,
    }
    write(artifacts / "verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
