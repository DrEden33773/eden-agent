#!/usr/bin/env python3
"""Installed S3 resources, FFF, package commands and independent native interop."""

import copy
import ctypes
import hashlib
import http.server
import json
import os
import pathlib
import platform
import runpy
import shutil
import ssl
import sys
import tarfile
import tempfile
import threading
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT, Composition, Package, library, package, target
from verification import author_artifact, author_sources, example, prepare
from verification import installed as prepared_install
from verification import openssl as resolve_openssl

helpers = runpy.run_path(str(ROOT / "scripts/verify-context.py"))
run = helpers["run"]
write = helpers["write"]
Server = helpers["Server"]
complete = helpers["complete"]
answer = helpers["answer"]
records = helpers["records"]


def author_packages(destination: pathlib.Path, scratch: pathlib.Path) -> list[Package]:
    author_sources(scratch, ["service-a", "service-b"])
    output = []
    services = {
        "service-a": ["example.compute.v1", "eden.resource-source.v1", "eden.instance-stop.v1"],
        "service-b": [
            "example.client.v1",
            "example.commands.v1",
            "example.command.v1",
            "example.input.v1",
            "example.tool.v1",
        ],
    }
    for name, contracts in services.items():
        artifact = library("author_" + name.replace("-", "_"))
        folder = destination / "plugins" / name
        folder.mkdir(parents=True, exist_ok=True)
        shutil.copy2(author_artifact(name), folder / artifact)
        selected = package(name, contracts, str(folder / artifact), target())
        if name == "service-b":
            selected["requires"] = ["example.compute.v1"]
        output.append(selected)
    return output


def stopped(pid: int) -> bool:
    if sys.platform == "win32":
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.restype = ctypes.c_void_p
        handle = kernel.OpenProcess(0x1000, False, pid)
        if not handle:
            return True
        status = ctypes.c_ulong()
        try:
            if not kernel.GetExitCodeProcess(ctypes.c_void_p(handle), ctypes.byref(status)):
                raise OSError(ctypes.get_last_error())
            return status.value != 259
        finally:
            kernel.CloseHandle(ctypes.c_void_p(handle))
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    return False


def tls_fixture(root: pathlib.Path) -> pathlib.Path:
    """Generate disposable keys at execution time; no private key is committed."""
    openssl = resolve_openssl()
    root.mkdir()
    config = root / "cert.cnf"
    config.write_text(
        "[req]\ndistinguished_name=dn\nprompt=no\n[dn]\nCN=Eden disposable verifier\n[ca]\nbasicConstraints=critical,CA:true\nkeyUsage=critical,keyCertSign,cRLSign\n[server]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1,DNS:localhost\n",
        encoding="utf-8",
    )
    run(
        [
            openssl,
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            root / "ca.key",
            "-out",
            root / "ca-cert.pem",
            "-days",
            "2",
            "-config",
            config,
            "-extensions",
            "ca",
        ],
        root,
    )
    run(
        [
            openssl,
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            root / "localhost-key.pem",
            "-out",
            root / "localhost.csr",
            "-config",
            config,
        ],
        root,
    )
    run(
        [
            openssl,
            "x509",
            "-req",
            "-in",
            root / "localhost.csr",
            "-CA",
            root / "ca-cert.pem",
            "-CAkey",
            root / "ca.key",
            "-CAcreateserial",
            "-out",
            root / "localhost-cert.pem",
            "-days",
            "2",
            "-extfile",
            config,
            "-extensions",
            "server",
        ],
        root,
    )
    return root


def default_search_loop(
    destination: pathlib.Path, scratch: pathlib.Path, base: Composition
) -> dict[str, Any]:
    project = scratch / "default-search"
    path = project / "src/a/long/relative/path/to/component.txt"
    path.parent.mkdir(parents=True)
    path.write_text("".join(f"needle row {n}\n" for n in range(1, 38)), encoding="utf-8")
    run(["git", "init", project], scratch)
    pages: list[str] = []

    def respond(body: dict[str, Any], index: int) -> tuple[int, dict[str, Any]]:
        arguments: dict[str, Any] = {"pattern": "needle", "limit": 20}
        if index:
            outputs = [item for item in body["input"] if item["type"] == "function_call_output"]
            output = outputs[-1]["output"]
            result = json.loads(output)
            text = result["text"]
            pages.append(text)
            header = json.loads(text.splitlines()[0])
            assert header["complete"] and header["total_matches"] == 37, result
            if not header["has_more"]:
                return 200, complete(answer("PAGES-COMPLETE"))
            arguments["cursor"] = header["cursor"]
        return 200, complete(
            [
                {
                    "type": "function_call",
                    "call_id": f"page-{index}",
                    "name": "grep",
                    "arguments": json.dumps(arguments),
                }
            ]
        )

    server = Server(respond)
    try:
        selected = copy.deepcopy(base)
        for package in selected["packages"]:
            if package["descriptor"]["package"] == "coding-tools":
                assert isinstance(package["config"], dict)
                package["config"]["tools"] = ["grep"]
        config = helpers["configure"](destination, selected, server, "default-search")
        host = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
        run(
            [
                host,
                "--composition",
                config,
                "--cwd",
                project,
                "--global-dir",
                scratch / "search-global",
                "--json",
                "Find needle and retrieve every page.",
            ],
            scratch,
        )
        assert len(pages) == 2 and len(server.requests) == 3
        histories = list((project / ".eden/sessions").glob("*.jsonl"))
        assert (
            len(histories) == 1
            and len([r for r in records(histories[0]) if r["kind"] == "tool_result"]) == 2
        )
        # Identical results and metadata; the comparator only repeats the path per row.
        lines = pages[0].splitlines()
        grouped_path = lines[1].removesuffix(":")
        flat = lines[0] + "\n" + "\n".join(grouped_path + ":" + line for line in lines[2:]) + "\n"
        return {"default_history_pagination": 37, "grouped": pages[0], "flat": flat}
    finally:
        server.close()


def main() -> None:
    os.environ["EDEN_CONTEXT_KEY"] = "context-verifier-key"
    resolve_openssl()
    prepare()
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-workspace-") as temp:
        scratch = pathlib.Path(temp)
        # A download receipt beside an extracted distribution is ordinary user
        # data. Every installed scenario must work without adopting it as a package.
        (scratch / "receipt.json").write_text('{"download_digest":"unrelated"}', encoding="utf-8")
        destination = prepared_install(scratch / "installation")
        suffix = ".exe" if sys.platform == "win32" else ""
        host = destination / "bin" / ("eden" + suffix)
        probe = example("workspace_probe")
        fixed_host = hashlib.sha256(host.read_bytes()).hexdigest()
        base: Composition = json.loads(
            (destination / "composition.json").read_text(encoding="utf-8")
        )
        results["unrelated_ancestor_receipt"] = (
            "All installed scenarios run beneath an unrelated download receipt"
        )
        project = scratch / "project 中文"
        project.mkdir()
        global_dir = project / "global"
        (global_dir / "skills/check").mkdir(parents=True)
        (global_dir / "prompts").mkdir()
        skill = global_dir / "skills/check/SKILL.md"
        skill.write_text(
            "---\nname: check\ndescription: Check metadata marker\n---\nSKILL-BODY-ONLY-ON-DEMAND",
            encoding="utf-8",
        )
        (global_dir / "prompts/greet.md").write_text(
            "Template marker: $1; $ARGUMENTS", encoding="utf-8"
        )
        (project / "AGENTS.md").write_text("FIRST-INSTRUCTION", encoding="utf-8")
        (project / ".eden").mkdir()
        (project / ".ignore").write_text("global/\n.eden/\n*.jsonl\n", encoding="utf-8")
        (project / ".eden/settings.json").write_text("{invalid untrusted project", encoding="utf-8")

        def command(
            name: str,
            *args: str | pathlib.Path,
            config: pathlib.Path | None = None,
            check: bool = True,
        ) -> Any:
            return run(
                [
                    host,
                    name,
                    *args,
                    "--composition",
                    config or destination / "composition.json",
                    "--cwd",
                    project,
                    "--global-dir",
                    global_dir,
                ],
                scratch,
                check=check,
            )

        resource = json.loads(command("resources", "list").stdout)
        assert "FIRST-INSTRUCTION" in resource["instructions"]
        assert "SKILL-BODY-ONLY-ON-DEMAND" not in json.dumps(resource)
        assert resource["skills"][0]["name"] == "check"
        results["untrusted_malformed_settings_ignored"] = True
        (project / ".eden/settings.json").unlink()
        results["resource_reload"] = json.loads(
            run([probe, destination / "composition.json", project, "resources"], scratch).stdout
        )
        results["search"] = json.loads(
            run([probe, destination / "composition.json", project, "search"], scratch).stdout
        )
        assert all(stopped(pid) for pid in results["search"]["independent_worker_pids"])
        results["search"]["worker_exit_observed"] = True

        results["search_cancel"] = json.loads(
            run([probe, destination / "composition.json", project, "cancel-search"], scratch).stdout
        )
        assert stopped(results["search_cancel"]["stopped_worker_pid"])
        # The installed host and probe are already fixed; only SDK and author sources follow.
        results["default_search_loop"] = default_search_loop(destination, scratch, base)
        authors = author_packages(destination, scratch)
        assert hashlib.sha256(host.read_bytes()).hexdigest() == fixed_host
        selected = copy.deepcopy(base)
        selected["packages"] = [
            p for p in selected["packages"] if p["descriptor"]["package"] != "workspace-resources"
        ] + authors
        marker = scratch / "author-a-stopped"
        for p in selected["packages"]:
            name = p["descriptor"]["package"]
            if name == "service-a":
                p["config"] = {"stop_marker": str(marker)}
            if name == "contributions":
                p["config"] = {
                    "commands": [
                        {"catalog": "example.commands.v1", "execute": "example.command.v1"}
                    ],
                    "input_hooks": ["example.input.v1"],
                    "tool_hooks": ["example.tool.v1"],
                }
        for author in authors:
            for contract in author["descriptor"]["provides"]:
                if contract != "eden.instance-stop.v1":
                    selected["roles"][contract] = author["descriptor"]["package"]
        interop = destination / "interop.json"
        write(interop, selected)
        results["interop"] = json.loads(
            run([probe, interop, project, "interop", marker], scratch).stdout
        )
        rollback = copy.deepcopy(selected)
        for package in rollback["packages"]:
            if package["descriptor"]["package"] == "service-a":
                package["config"] = {"stop_marker": str(scratch / "absent-parent/stop")}
            if package["descriptor"]["package"] == "service-b":
                package["descriptor"]["version"] = "wrong"
        rollback_path = destination / "rollback.json"
        write(rollback_path, rollback)
        failure = command("commands", config=rollback_path, check=False)
        assert (
            failure.returncode
            and "IncompatibleContract (loader)" in failure.stderr
            and "CleanupFailure (service-a)" in failure.stderr
        ), failure.stderr
        results["initialization_and_finalizer_failure_visible"] = True
        untrusted_marker = scratch / "untrusted-project-effect"
        write(
            project / ".eden/settings.json",
            {"plugins": {"service-a": {"stop_marker": str(untrusted_marker)}}},
        )
        rejected = command("commands", config=interop)
        assert not untrusted_marker.exists() and "untrusted" in rejected.stderr.lower()
        command("trust", "allow", project)
        command("commands", config=interop)
        assert untrusted_marker.is_file()
        command("trust", "deny", project)
        (project / ".eden/settings.json").unlink()
        results["project_configuration_trust_gate"] = True
        # A streamed run keeps the trust diagnostic on stderr, exactly once, and
        # leaves stdout a clean event stream: one outlet owns every human line.
        write(project / ".eden/settings.json", {"plugins": {}})
        server = Server(lambda _body, _index: (200, complete(answer("UNTRUSTED-STREAM"))))
        try:
            streamed = run(
                [
                    host,
                    "--composition",
                    helpers["configure"](destination, selected, server, "untrusted-stream"),
                    "--cwd",
                    project,
                    "--global-dir",
                    global_dir,
                    "--session",
                    scratch / "untrusted-stream.jsonl",
                    "--json",
                    "Reply briefly",
                ],
                scratch,
                check=False,
            )
        finally:
            server.close()
        (project / ".eden/settings.json").unlink()
        diagnostics = [
            line
            for line in streamed.stderr.splitlines()
            if "Untrusted project resources ignored" in line
        ]
        assert len(diagnostics) == 1 and diagnostics[0].startswith("warning: "), streamed.stderr
        events = [json.loads(line) for line in streamed.stdout.splitlines() if line.strip()]
        assert events and all(event["kind"] for event in events), streamed.stdout
        # The level is in the data, not only in the rendering, so a --json
        # consumer reads the same judgement the terminal shows.
        reports = [event for event in events if event["kind"] == "resource_diagnostic"]
        assert len(reports) == 1, streamed.stdout
        assert reports[0]["payload"]["level"] == "warning", reports[0]
        assert "Untrusted project resources ignored" in reports[0]["payload"]["message"], reports[0]
        results["json_stream_keeps_one_level_diagnostic"] = True

        bad = copy.deepcopy(selected)
        for p in bad["packages"]:
            if p["descriptor"]["package"] == "service-b":
                p["requires"] = ["example.compute.v2"]
        mismatch = destination / "mismatch.json"
        write(mismatch, bad)
        assert command("commands", config=mismatch, check=False).returncode != 0
        failed = copy.deepcopy(selected)
        for p in failed["packages"]:
            if p["descriptor"]["package"] == "service-a":
                p["config"] = {"fail_init": True}
        failed_path = destination / "failed-init.json"
        write(failed_path, failed)
        results["switch"] = json.loads(
            run([probe, interop, project, "switch", mismatch, failed_path], scratch).stdout
        )

        def respond(_body: dict[str, Any], index: int) -> tuple[int, dict[str, Any]]:
            if index == 0:
                return 200, complete(
                    [
                        {
                            "type": "function_call",
                            "call_id": "hook-write",
                            "name": "write",
                            "arguments": json.dumps(
                                {"path": "hook-result.txt", "content": "original-model-content"}
                            ),
                        }
                    ]
                )
            return 200, complete(answer("HOOK-TASK-COMPLETE"))

        server = Server(respond)
        try:
            config = helpers["configure"](destination, selected, server, "hook-task")
            history = project / "hooks.jsonl"
            run(
                [
                    host,
                    "--composition",
                    config,
                    "--cwd",
                    project,
                    "--global-dir",
                    global_dir,
                    "--session",
                    history,
                    "--json",
                    "Exercise hooks",
                ],
                scratch,
            )
            assert (project / "hook-result.txt").read_text(
                encoding="utf-8"
            ) == "external-tool-hook\n"
            assert "external-a" in json.dumps(
                server.requests[0]
            ) and "external-input-hook" in json.dumps(server.requests[0])
            saved = records(history)
            intents = [r for r in saved if r["kind"] == "tool_execution_intent"]
            assert (
                intents
                and "original-model-content" in json.dumps(intents)
                and "external-tool-hook" in json.dumps(intents)
            )
            results["hooks_actual_model_and_tool"] = True
        finally:
            server.close()

        # An installed outer hook with an unresolved inner route must fail closed.
        for stage in ["input_hooks", "tool_hooks"]:
            sentinel = project / "hook-result.txt"
            sentinel.unlink(missing_ok=True)
            server = Server(respond)
            try:
                broken = copy.deepcopy(selected)
                for package in broken["packages"]:
                    if package["descriptor"]["package"] == "contributions":
                        assert isinstance(package["config"], dict)
                        package["config"][stage] = ["example.missing.v1"]
                config = helpers["configure"](destination, broken, server, "missing-" + stage)
                result = run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--global-dir",
                        global_dir,
                        "--no-session",
                        "--json",
                        "Exercise broken hook",
                    ],
                    scratch,
                    check=False,
                )
                assert "example.missing.v1" in result.stdout + result.stderr, (
                    stage,
                    result.returncode,
                    result.stderr,
                    result.stdout[-2000:],
                )
                assert bool(result.returncode) == (stage == "input_hooks")
                assert not sentinel.exists()
                assert len(server.requests) == (0 if stage == "input_hooks" else 2)
            finally:
                server.close()
        results["missing_inner_hook_blocks_effect"] = True

        if sys.platform == "win32":

            def powershell_reply(_body: dict[str, Any], index: int) -> tuple[int, dict[str, Any]]:
                if index == 0:
                    return 200, complete(
                        [
                            {
                                "type": "function_call",
                                "call_id": "native-pwsh",
                                "name": "powershell",
                                "arguments": json.dumps(
                                    {
                                        "command": "[System.IO.File]::WriteAllText((Join-Path (Get-Location) 'pwsh-cwd.txt'), (Get-Location).Path)"
                                    }
                                ),
                            }
                        ]
                    )
                return 200, complete(answer("POWERSHELL-COMPLETE"))

            server = Server(powershell_reply)
            try:
                powershell = copy.deepcopy(base)
                for package in powershell["packages"]:
                    if package["descriptor"]["package"] == "coding-tools":
                        assert isinstance(package["config"], dict)
                        package["config"]["tools"] = ["powershell"]
                config = helpers["configure"](destination, powershell, server, "powershell")
                run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--global-dir",
                        global_dir,
                        "--no-session",
                        "--json",
                        "Run the PowerShell task.",
                    ],
                    scratch,
                )
                assert (
                    pathlib.Path((project / "pwsh-cwd.txt").read_text(encoding="utf-8")).resolve()
                    == project.resolve()
                )
                results["installed_native_powershell_unicode_cwd"] = True
            finally:
                server.close()

        source = scratch / "bundle"
        source.mkdir()
        artifact = pathlib.Path(authors[0]["library"])
        shutil.copy2(artifact, source / artifact.name)
        manifest = copy.deepcopy(authors[0])
        manifest["library"] = artifact.name
        manifest["config"] = None
        write(source / "package.json", {"manifest": manifest, "dependencies": [], "build": None})
        installed = json.loads(command("package", "install", source).stdout)
        archive = scratch / "bundle.tar.gz"
        with tarfile.open(archive, "w:gz") as output:
            output.add(source, arcname="bundle")
        assert (
            json.loads(command("package", "install", archive).stdout)["digest"]
            == installed["digest"]
        )
        run(["git", "init", str(source)], scratch)
        run(["git", "-C", source, "add", "."], scratch)
        run(
            [
                "git",
                "-C",
                source,
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Package fixture",
            ],
            scratch,
        )
        commit = run(["git", "-C", source, "rev-parse", "HEAD"], scratch).stdout.strip()
        git_source = json.dumps({"kind": "git", "url": str(source), "revision": commit})
        locked = json.loads(command("package", "install", "--source-json", git_source).stdout)
        assert locked["source"]["commit"] == commit
        results["package_local_archive_fixed_git"] = True

        # Copy using a different Store composition; the copied binding still owns v1.
        managed = copy.deepcopy(selected)
        for package in managed["packages"]:
            if package["descriptor"]["package"] == "service-a":
                package.update(copy.deepcopy(installed["manifest"]))
                package["library"] = str(
                    pathlib.Path(installed["path"]) / installed["manifest"]["library"]
                )
        server = Server(lambda _body, _index: (200, complete(answer("MANAGED-SESSION"))))
        source_history = scratch / "managed-source.jsonl"
        try:
            config = helpers["configure"](destination, managed, server, "managed-session")
            run(
                [
                    host,
                    "--composition",
                    config,
                    "--cwd",
                    project,
                    "--global-dir",
                    global_dir,
                    "--session",
                    source_history,
                    "--json",
                    "Reply briefly",
                ],
                scratch,
            )
        finally:
            server.close()
        # Both ordinary startup and the isolated Store used for copies must keep
        # the declared managed location until its receipt has been verified.
        guarded = copy.deepcopy(base)
        guard_root = scratch / "guard-store/packages/workspace-resources/0.1.0" / target()
        guard_lib = guard_root / "lib"
        guard_lib.mkdir(parents=True)
        outside = None
        for item in guarded["packages"]:
            actual = destination / item["library"]
            item["library"] = str(actual)
            if item["descriptor"]["package"] == "workspace-resources":
                outside = actual.parent
                shutil.copy2(actual, guard_lib / actual.name)
                item["library"] = str(guard_lib / actual.name)
        assert outside is not None
        (guard_root / "receipt.json").write_text("invalid receipt", encoding="utf-8")
        guard_config = scratch / "guard-composition.json"
        write(guard_config, guarded)
        for state in ("regular", "redirect"):
            if state == "redirect":
                shutil.rmtree(guard_lib)
                if sys.platform == "win32":
                    run(["cmd", "/C", "mklink", "/J", guard_lib, outside], scratch)
                else:
                    guard_lib.symlink_to(outside, target_is_directory=True)
            for operation in ("resources", "copy"):
                rejected_copy = scratch / f"guard-{state}-copy.jsonl"
                denied = (
                    command("resources", "list", config=guard_config, check=False)
                    if operation == "resources"
                    else command(
                        "session",
                        "fork",
                        source_history,
                        rejected_copy,
                        "--apply",
                        config=guard_config,
                        check=False,
                    )
                )
                assert denied.returncode != 0 and "PackageIntegrity" in denied.stderr
                assert not rejected_copy.exists()
        if sys.platform == "win32":
            guard_lib.rmdir()
        else:
            guard_lib.unlink()
        results["managed_redirect_integrity"] = {
            "ordinary_startup_and_isolated_copy_reject": True,
            "regular_and_redirected_library_checked": True,
            "directory_link": "junction" if sys.platform == "win32" else "symlink",
        }
        copied = scratch / "managed-fork.jsonl"
        recovered = scratch / "managed-recovery.jsonl"
        command("session", "fork", source_history, copied, "--apply")
        command("session", "recover", source_history, recovered, "--apply")
        source_history.unlink()
        references = []
        for reference in (global_dir / "distribution/sessions").glob("*.json"):
            value = json.loads(reference.read_text(encoding="utf-8"))
            if pathlib.Path(value["history"]).exists() and any(
                pathlib.Path(value["history"]).samefile(p) for p in [copied, recovered]
            ):
                assert any(
                    pathlib.Path(p).samefile(
                        pathlib.Path(installed["path"]) / installed["manifest"]["library"]
                    )
                    for p in value["libraries"]
                )
                references.append(value["history"])
        assert len(references) == 2
        assert all(
            any(pathlib.Path(ref).samefile(history) for ref in references)
            for history in [copied, recovered]
        )
        rejected = command("package", "remove", "service-a", "0.1.0", check=False)
        assert (
            rejected.returncode
            and "PackageInUse (distribution)" in rejected.stderr
            and all(reference in rejected.stderr for reference in references)
        ), rejected.stderr
        results["copied_saved_binding_references"] = True

        relocated_global = scratch / "relocated-global"
        relocated_global.mkdir()
        managed_library = pathlib.Path(installed["path"]) / installed["manifest"]["library"]
        # Determine containment while both paths exist; aliases/verbatim prefixes are identities,
        # not textual prefixes. Never replace a substring in a canonical package path.
        global_parent = next(
            parent for parent in managed_library.parents if parent.samefile(global_dir)
        )
        relative_library = managed_library.relative_to(global_parent)
        shutil.move(global_dir / "distribution", relocated_global / "distribution")
        relocated = copy.deepcopy(managed)
        new_library = relocated_global / relative_library
        for package in relocated["packages"]:
            if package["descriptor"]["package"] == "service-a":
                package["library"] = str(new_library)
        server = Server(lambda _body, _index: (200, complete(answer("RELOCATED-SESSION"))))
        try:
            config = helpers["configure"](destination, relocated, server, "relocated-session")
            run(
                [
                    host,
                    "--composition",
                    config,
                    "--global-dir",
                    relocated_global,
                    "--session",
                    copied,
                    "--json",
                    "Reply briefly",
                ],
                scratch,
            )
        finally:
            server.close()
        newest_lock = [r for r in records(copied) if r["kind"] == "composition_lock"][-1]
        assert any(
            pathlib.Path(p).samefile(new_library)
            for p in newest_lock["payload"]["library_locations"]
        )
        relocated_fork = scratch / "relocated-fork.jsonl"
        command("session", "fork", copied, relocated_fork, "--apply")
        copied.unlink()
        recovered.unlink()
        rejected = run(
            [
                host,
                "package",
                "remove",
                "service-a",
                "0.1.0",
                "--composition",
                destination / "composition.json",
                "--global-dir",
                relocated_global,
                "--cwd",
                project,
            ],
            scratch,
            check=False,
        )
        relocated_references = [
            json.loads(reference.read_text(encoding="utf-8"))["history"]
            for reference in (relocated_global / "distribution/sessions").glob("*.json")
        ]
        relocated_references = [
            ref
            for ref in relocated_references
            if pathlib.Path(ref).exists() and pathlib.Path(ref).samefile(relocated_fork)
        ]
        assert len(relocated_references) == 1
        assert (
            rejected.returncode
            and "PackageInUse (distribution)" in rejected.stderr
            and relocated_references[0] in rejected.stderr
        ), rejected.stderr
        results["relocated_reopen_copy_references"] = True

        # TLS remains verified against an explicit test CA, with digest and downgrade checks.
        class ArchiveHandler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                if self.path == "/downgrade":
                    self.send_response(302)
                    self.send_header("Location", "http://127.0.0.1/forbidden")
                    self.end_headers()
                    return
                self.send_response(200)
                payload = archive.read_bytes()
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        tls = FixtureHTTPServer(("127.0.0.1", 0), ArchiveHandler)
        certs = tls_fixture(scratch / "tls")
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(certs / "localhost-cert.pem", certs / "localhost-key.pem")
        tls.socket = context.wrap_socket(tls.socket, server_side=True)
        thread = threading.Thread(target=tls.serve_forever, daemon=True)
        thread.start()
        tls_config = copy.deepcopy(base)
        for p in tls_config["packages"]:
            if p["descriptor"]["package"] == "distribution":
                p["config"] = {"ca_certificate": str(certs / "ca-cert.pem")}
        tls_path = destination / "tls.json"
        write(tls_path, tls_config)
        try:
            source_url = {
                "kind": "https",
                "url": f"https://127.0.0.1:{tls.server_port}/bundle",
                "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            }
            receipt = json.loads(
                command(
                    "package", "install", "--source-json", json.dumps(source_url), config=tls_path
                ).stdout
            )
            assert receipt["digest"] == installed["digest"]
            source_url["sha256"] = "0" * 64
            assert (
                "ChecksumMismatch"
                in command(
                    "package",
                    "install",
                    "--source-json",
                    json.dumps(source_url),
                    config=tls_path,
                    check=False,
                ).stderr
            )
            source_url["url"] = f"https://127.0.0.1:{tls.server_port}/downgrade"
            assert (
                command(
                    "package",
                    "install",
                    "--source-json",
                    json.dumps(source_url),
                    config=tls_path,
                    check=False,
                ).returncode
                != 0
            )
            results["https_verified_digest_and_no_downgrade"] = True
        finally:
            tls.shutdown()
            tls.server_close()
            thread.join()

        build_source = scratch / "build-source"
        shutil.copytree(scratch / "sdk", build_source / "sdk")
        shutil.copytree(scratch / "authors/service-a", build_source / "authors/service-a")
        shutil.copy2(ROOT / "rust-toolchain.toml", build_source / "rust-toolchain.toml")
        source_manifest = copy.deepcopy(manifest)
        source_manifest["target"] = "source-only"
        write(
            build_source / "package.json",
            {
                "manifest": source_manifest,
                "dependencies": [],
                "build": {
                    "manifest_path": "authors/service-a/Cargo.toml",
                    "artifact": f"authors/service-a/target/release/{artifact.name}",
                },
            },
        )
        # A separate isolated store keeps the prebuilt version distinct from this source receipt.
        build_global = scratch / "build-global"
        build_command = [
            host,
            "package",
            "install",
            build_source,
            "--composition",
            destination / "composition.json",
            "--global-dir",
            build_global,
            "--cwd",
            project,
        ]
        assert "BuildRequired" in run(build_command, scratch, check=False).stderr
        built = json.loads(run([*build_command, "--build"], scratch).stdout)
        assert pathlib.Path(built["path"]).joinpath(built["manifest"]["library"]).is_file()
        assert not list(build_source.rglob("*eden_agent*"))
        results["explicit_source_plugin_build"] = True
        assert hashlib.sha256(host.read_bytes()).hexdigest() == fixed_host
    results["target"] = target()
    results["platform"] = platform.platform()
    results["fixed_host_sha256"] = fixed_host
    write(ROOT / "artifacts/workspace-verification.json", results)
    print(json.dumps(results, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
