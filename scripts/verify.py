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
from install import ROOT, ROLES, install, library, package, target


def run(args, cwd, check=True):
    result = subprocess.run([str(arg) for arg in args], cwd=cwd, text=True, encoding="utf-8", capture_output=True, timeout=240)
    if check and result.returncode:
        raise RuntimeError(f"{args}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def main():
    run(["cargo", "build", "--workspace", "--locked"], ROOT)
    run(["cargo", "build", "-p", "eden-agent", "--examples", "--locked"], ROOT)
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = install(artifacts / "install")
    suffix = ".exe" if sys.platform == "win32" else ""
    for example in ["embedded", "contract_probe", "initialization_probe"]:
        shutil.copy2(ROOT / "target/debug/examples" / (example + suffix), destination / "bin" / (example + suffix))
    fixed = {path.name: path.read_bytes() for path in (destination / "bin").iterdir() if path.is_file()}
    host = destination / "bin" / ("eden" + suffix)
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results = {}
    triple = target()
    with tempfile.TemporaryDirectory(prefix="eden-independent-authors-") as temp:
        scratch = pathlib.Path(temp)
        # This tree contains the public SDK and protocol, with no kernel, CLI or first-party plugin source.
        sdk = scratch / "sdk"
        (sdk / "crates").mkdir(parents=True)
        for crate in ["eden-protocol", "eden-plugin-sdk"]:
            shutil.copytree(ROOT / "crates" / crate, sdk / "crates" / crate)
        manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        manifest = manifest.replace('members = ["crates/*", "plugins/standard"]', 'members = ["crates/*"]')
        manifest = "\n".join(line for line in manifest.splitlines() if not line.startswith("exclude =") and not line.startswith("eden-kernel =") and not line.startswith("eden-agent =")) + "\n"
        (sdk / "Cargo.toml").write_text(manifest, encoding="utf-8")
        shutil.copy2(ROOT / "rust-toolchain.toml", scratch / "rust-toolchain.toml")
        for name, roles in [("loop-a", [ROLES[0]]), ("context-b", [ROLES[1]]), ("lifecycle", [ROLES[0]]), ("init-probe", [ROLES[0]])]:
            author = scratch / "authors" / name
            shutil.copytree(ROOT / "tests/contract-authors" / name, author, ignore=shutil.ignore_patterns("target"))
            cargo = author / "Cargo.toml"
            cargo.write_text(cargo.read_text(encoding="utf-8").replace("../../../crates/eden-plugin-sdk", "../../sdk/crates/eden-plugin-sdk"), encoding="utf-8")
            run(["cargo", "build", "--locked", "--target-dir", scratch / "author-target"], author)
            lib = library("author_" + name.replace("-", "_"))
            plugin_dir = destination / "plugins" / name / "0.1.0"
            plugin_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy2(scratch / "author-target/debug" / lib, plugin_dir / lib)
            composition["packages"].append(package(name, roles, f"plugins/{name}/0.1.0/{lib}", triple))
        invalid_author = scratch / "authors" / "invalid"
        shutil.copytree(ROOT / "tests/contract-authors/invalid", invalid_author, ignore=shutil.ignore_patterns("target"))
        cargo = invalid_author / "Cargo.toml"
        cargo.write_text(cargo.read_text(encoding="utf-8").replace("../../../crates/eden-plugin-sdk", "../../sdk/crates/eden-plugin-sdk"), encoding="utf-8")
        invalid_paths = {}
        for feature in ["wrong-abi", "wrong-sdk", "short-table", "metadata-panic", "init-panic"]:
            run(["cargo", "build", "--locked", "--features", feature, "--target-dir", scratch / "author-target"], invalid_author)
            lib = library("author_invalid")
            invalid_dir = destination / "plugins" / feature
            invalid_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy2(scratch / "author-target/debug" / lib, invalid_dir / lib)
            invalid_paths[feature] = f"plugins/{feature}/{lib}"
        caller = scratch / "unrelated caller 工作目录"
        caller.mkdir()
        base = copy.deepcopy(composition)
        # Only explicitly used packages are enabled; lifecycle requires its test-owned endpoint.
        base["packages"] = [p for p in base["packages"] if p["descriptor"]["package"] not in ["lifecycle", "init-probe"]]
        for name, loop, context, expected, order in [
            ("default", "standard", "standard", "standard:hello => echo[standard:hello]", [ROLES[0], ROLES[1], ROLES[2], ROLES[3], ROLES[2]]),
            ("loop_a", "loop-a", "standard", "A/standard:hello => echo[A:standard:hello]", [ROLES[0], ROLES[1], ROLES[3], ROLES[2]]),
            ("context_b", "standard", "context-b", "B<HELLO> => echo[B<HELLO>]", [ROLES[0], ROLES[1], ROLES[2], ROLES[3], ROLES[2]]),
            ("mixed", "loop-a", "context-b", "A/B<HELLO> => echo[A:B<HELLO>]", [ROLES[0], ROLES[1], ROLES[3], ROLES[2]]),
        ]:
            selected = copy.deepcopy(base)
            selected["roles"][ROLES[0]] = loop
            selected["roles"][ROLES[1]] = context
            path = destination / f"{name}.json"
            write(path, selected)
            events = [json.loads(line) for line in run([host, "--composition", path, "--json", "hello"], caller).stdout.splitlines()]
            assert events[0]["kind"] == "accepted" and events[-1]["kind"] == "settled"
            assert events[-1]["payload"] == {"outcome": {"status": "completed", "value": expected}, "cleanup_errors": []}
            assert [e["payload"]["contract"] for e in events if e["kind"] == "service_called"] == order
            assert [e["sequence"] for e in events] == list(range(1, len(events) + 1))
            assert len({(e["session_id"], e["run_id"]) for e in events}) == 1
            model_inputs = [e["payload"] for e in events if e["kind"] == "model_input"]
            assert model_inputs[-1]["tool_result"] in expected
            assert run([host, "--composition", path, "--print", "hello"], destination).stdout.strip() == expected
            results[name] = {"result": expected, "order": order, "model_inputs": model_inputs}
        assert run([host, "hello"], caller).stdout.strip() == "standard:hello => echo[standard:hello]"
        embedded = json.loads(run([destination / "bin" / ("embedded" + suffix), destination / "composition.json"], caller).stdout)
        assert embedded["outcome"]["value"] == results["default"]["result"]
        results["embedded"] = embedded
        for feature, lib in invalid_paths.items():
            selected = copy.deepcopy(base)
            selected["packages"].append(package("invalid", [ROLES[0]], lib, triple))
            path = destination / f"{feature}.json"
            write(path, selected)
            result = run([host, "--composition", path, "hello"], caller, check=False)
            expected = "PluginFailure" if "panic" in feature else "IncompatibleContract"
            assert result.returncode != 0 and expected in result.stderr, (feature, result.stderr)
            results[feature] = expected
        initialized = copy.deepcopy(base)
        probe_package = next(p for p in composition["packages"] if p["descriptor"]["package"] == "init-probe")
        initialized["packages"].append(copy.deepcopy(probe_package))
        initialized["roles"][ROLES[0]] = "init-probe"
        path = destination / "init-probe.json"
        write(path, initialized)
        results["abandoned_init"] = json.loads(run([destination / "bin" / ("initialization_probe" + suffix), path], caller).stdout)
        marker = scratch / "destroy-marker"
        initialized["packages"][-1]["config"] = {"marker": str(marker)}
        write(path, initialized)
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run([str(host), "--composition", str(path), "--print", "hello"], cwd=caller, stdout=writer, stderr=subprocess.PIPE, timeout=20)
        finally:
            os.close(writer)
        assert result.returncode != 0 and marker.read_text() == "destroyed"
        results["broken_stdout"] = "error returned after native instance destruction"
        rollback_marker = scratch / "rollback-marker"
        initialized["packages"][-1]["config"] = {"marker": str(rollback_marker)}
        initialized["packages"].append(package("invalid", [ROLES[0]], invalid_paths["init-panic"], triple))
        write(path, initialized)
        result = run([host, "--composition", path, "hello"], caller, check=False)
        assert result.returncode != 0 and "PluginFailure" in result.stderr
        assert rollback_marker.read_text() == "destroyed"
        results["partial_initialization_rollback"] = "previous native instance destroyed"
        for mode in ["complete", "cancel", "dropped_receiver", "cleanup_error", "failed_then_cancel", "panic", "drop_panic"]:
            selected = copy.deepcopy(composition)
            selected["packages"] = [p for p in selected["packages"] if p["descriptor"]["package"] != "init-probe"]
            selected["roles"][ROLES[0]] = "lifecycle"
            path = destination / "lifecycle.json"
            write(path, selected)
            result = run([destination / "bin" / ("contract_probe" + suffix), path, mode], caller)
            results[mode] = json.loads(result.stdout)
        for field, value, expected_code in [("sdk", "wrong", "IncompatibleContract"), ("host", "wrong", "IncompatibleContract"), ("target", "wrong", "IncompatibleContract"), ("library", "absent.so", "MissingDependency"), ("config", {"fail_init": True}, "Unavailable")]:
            selected = copy.deepcopy(base)
            selected["packages"][0][field] = value
            path = destination / f"invalid-{field}.json"
            write(path, selected)
            result = run([host, "--composition", path, "hello"], caller, check=False)
            assert result.returncode != 0 and expected_code in result.stderr, (field, result.stderr)
            results[f"reject_{field}"] = expected_code
        selected = copy.deepcopy(base)
        del selected["roles"][ROLES[1]]
        path = destination / "missing-role.json"
        write(path, selected)
        result = run([host, "--composition", path, "hello"], caller, check=False)
        assert result.returncode != 0 and "MissingDependency" in result.stderr
        results["missing_role"] = "MissingDependency"
        assert all((destination / "bin" / name).read_bytes() == data for name, data in fixed.items())
    evidence = {"platform": platform.platform(), "target": triple, "rust": run(["rustc", "--version"], ROOT).stdout.strip(), "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(), "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()), "host_unchanged": True, "host_sha256": hashlib.sha256(fixed[host.name]).hexdigest(), "independent_author_build": True, "results": results}
    write(artifacts / "verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
