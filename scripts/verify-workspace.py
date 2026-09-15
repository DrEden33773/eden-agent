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
import re
import runpy
import shutil
import ssl
import sys
import tarfile
import tempfile
import threading
from typing import Any

from install import ROOT, Composition, Package, install, library, package, target

helpers = runpy.run_path(str(ROOT / "scripts/verify-context.py"))
run = helpers["run"]
write = helpers["write"]
Server = helpers["Server"]
complete = helpers["complete"]
answer = helpers["answer"]
records = helpers["records"]


def author_packages(destination: pathlib.Path, scratch: pathlib.Path) -> list[Package]:
    sdk = scratch / "sdk"
    for name in ["eden-protocol", "eden-plugin-sdk"]:
        shutil.copytree(ROOT / "crates" / name, sdk / "crates" / name)
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    manifest = re.sub(r"^members = .*$", 'members = ["crates/*"]', manifest, flags=re.M)
    manifest = (
        "\n".join(
            line
            for line in manifest.splitlines()
            if not line.startswith(("exclude =", "eden-kernel =", "eden-agent ="))
        )
        + "\n"
    )
    (sdk / "Cargo.toml").write_text(manifest, encoding="utf-8")
    shutil.copy2(ROOT / "rust-toolchain.toml", scratch / "rust-toolchain.toml")
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
        author = scratch / "authors" / name
        shutil.copytree(
            ROOT / "tests/contract-authors" / name, author, ignore=shutil.ignore_patterns("target")
        )
        cargo = author / "Cargo.toml"
        cargo.write_text(
            cargo.read_text(encoding="utf-8").replace(
                "../../../crates/eden-plugin-sdk", "../../sdk/crates/eden-plugin-sdk"
            ),
            encoding="utf-8",
        )
        run(["cargo", "build", "--locked", "--target-dir", scratch / "author-target"], author)
        artifact = library("author_" + name.replace("-", "_"))
        folder = destination / "plugins" / name
        folder.mkdir(parents=True, exist_ok=True)
        shutil.copy2(scratch / "author-target/debug" / artifact, folder / artifact)
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


def main() -> None:
    os.environ["EDEN_CONTEXT_KEY"] = "context-verifier-key"
    run(["cargo", "build", "--workspace", "--locked"], ROOT)
    run(["cargo", "build", "-p", "eden-agent", "--example", "workspace_probe", "--locked"], ROOT)
    destination = install(ROOT / "artifacts/workspace-install")
    suffix = ".exe" if sys.platform == "win32" else ""
    host = destination / "bin" / ("eden" + suffix)
    probe = destination / "bin" / ("workspace_probe" + suffix)
    shutil.copy2(ROOT / "target/debug/examples" / probe.name, probe)
    fixed_host = hashlib.sha256(host.read_bytes()).hexdigest()
    base: Composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-workspace-") as temp:
        scratch = pathlib.Path(temp)
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

        tls = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ArchiveHandler)
        certs = ROOT / "tests/fixtures/package-tls"
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
