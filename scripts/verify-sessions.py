#!/usr/bin/env python3
"""Installed S2 session evidence using public commands and an SDK-only author."""

import copy
import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import socket
import subprocess
import sys
import tempfile
from collections.abc import Callable, Sequence
from typing import Any, Protocol, cast

from install import ROOT, Composition, Package, target
from verification import installed, prepare


class CodingServer(Protocol):
    requests: list[dict[str, Any]]
    address: str

    def close(self) -> None: ...


class CodingHelpers(Protocol):
    """Typed interface to the shared verifier loaded from its CLI filename."""

    PROVIDER: str
    CONTEXT: str
    STORE: str
    TOOL: str

    def run(
        self,
        args: Sequence[str | pathlib.Path],
        cwd: pathlib.Path,
        check: bool = True,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]: ...
    def write(self, path: pathlib.Path, value: object) -> None: ...
    def Server(
        self,
        respond: Callable[[dict[str, Any], int], list[dict[str, Any]]],
        gated: bool = False,
        partial: bool = False,
    ) -> CodingServer: ...
    def answer(self, text: str) -> list[dict[str, Any]]: ...
    def call(self, identity: str, name: str, **arguments: str) -> list[dict[str, Any]]: ...
    def configure(
        self, destination: pathlib.Path, composition: Composition, server: CodingServer, name: str
    ) -> pathlib.Path: ...
    def events(
        self, result: subprocess.CompletedProcess[str], completed: bool = True
    ) -> list[dict[str, Any]]: ...
    def build_author(self, destination: pathlib.Path, scratch: pathlib.Path) -> Package: ...


spec = importlib.util.spec_from_file_location("coding_verifier", ROOT / "scripts/verify-coding.py")
assert spec is not None and spec.loader is not None, "coding verifier loader unavailable"
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
helpers = cast(CodingHelpers, module)
run, write, Server = helpers.run, helpers.write, helpers.Server
answer, call, configure, events = helpers.answer, helpers.call, helpers.configure, helpers.events
PROVIDER, CONTEXT, STORE = helpers.PROVIDER, helpers.CONTEXT, helpers.STORE
INTERPRETER, MIGRATOR = "eden.record-interpreter.v1", "eden.state-migrator.v1"


def records(path: pathlib.Path) -> list[dict[str, Any]]:
    result = []
    for line in path.read_text(encoding="utf-8").splitlines():
        value = json.loads(line)
        result.extend(value["transaction"] if "transaction" in value else [value])
    return result


def lines(result: subprocess.CompletedProcess[str]) -> list[dict[str, Any]]:
    return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def broken_stdout(
    host: pathlib.Path,
    selected: Composition,
    destination: pathlib.Path,
    server: CodingServer,
    history: pathlib.Path,
    caller: pathlib.Path,
    verb: str,
    arguments: Sequence[str],
) -> dict[str, Any]:
    """Close the output reader only after Store Open owns its writer lock."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(20)
        gated = copy.deepcopy(selected)
        gated["packages"][-1]["config"] = {"open_gate": f"127.0.0.1:{listener.getsockname()[1]}"}
        config = configure(destination, gated, server, f"broken-stdout-{verb}")
        process = subprocess.Popen(
            [
                str(host),
                "session",
                verb,
                str(history),
                *arguments,
                "--composition",
                str(config),
            ],
            cwd=caller,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        try:
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(20)
                with connection.makefile("rb") as incoming:
                    assert incoming.readline(128) == b"open-ready\n", (
                        "Store Open did not reach its ownership barrier"
                    )
                assert process.stdout is not None
                process.stdout.close()
                process.stdout = None
                connection.sendall(b"x")
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(20)
                with connection.makefile("rb") as incoming:
                    assert incoming.readline(128) == b"store-closed\n", (
                        "stdout error skipped the native Store Close barrier"
                    )
            _, stderr = process.communicate(timeout=20)
            assert process.returncode != 0, "a closed stdout reader must produce command failure"
            assert stderr.strip() and "panicked" not in stderr, stderr
            return {
                "command": verb,
                "exit_code": process.returncode,
                "store_open_before_reader_closed": True,
                "native_store_closed_after_output_failure": True,
                "error": stderr.strip(),
            }
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=20)
            if process.stderr is not None:
                process.stderr.close()


def main() -> None:
    os.environ["EDEN_VERIFY_KEY"] = "controlled-verifier-key"
    prepare()
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = installed(artifacts / "session-install")
    suffix = ".exe" if sys.platform == "win32" else ""
    host = destination / "bin" / ("eden" + suffix)
    probe = destination / "bin" / ("session_probe" + suffix)
    host_hash = digest(host)
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-session-acceptance-") as temp:
        scratch = pathlib.Path(temp)
        # Build the external author only after the installed host is fixed.
        author = helpers.build_author(destination, scratch)
        author["descriptor"]["provides"] = [
            PROVIDER,
            CONTEXT,
            helpers.TOOL,
            STORE,
            INTERPRETER,
            MIGRATOR,
        ]
        caller, project, relocated = [
            scratch / name
            for name in [
                "unrelated caller 工作目录",
                "original project",
                "relocated project",
            ]
        ]
        for folder in [caller, project, relocated]:
            folder.mkdir()

        def respond(body: dict[str, Any], index: int) -> list[dict[str, Any]]:
            if index == 0:
                return call(
                    "origin-write",
                    "write",
                    path="origin-effect.txt",
                    content="recorded cwd effect\n",
                )
            if "MOVED_CONTINUATION" in json.dumps(body["input"]) and not any(
                item.get("type") == "function_call_output" and item.get("call_id") == "move-write"
                for item in body["input"]
            ):
                return call(
                    "move-write",
                    "write",
                    path="moved-effect.txt",
                    content="relocated cwd effect\n",
                )
            return answer(f"controlled reply {index}")

        server = Server(respond)
        try:
            config = configure(destination, composition, server, "sessions")

            def command(
                *args: str | pathlib.Path, check: bool = True
            ) -> subprocess.CompletedProcess[str]:
                return run([host, *args], caller, check=check)

            def session(
                verb: str,
                history: pathlib.Path,
                *args: str | pathlib.Path,
                selected: pathlib.Path = config,
                check: bool = True,
            ) -> subprocess.CompletedProcess[str]:
                return command(
                    "session",
                    verb,
                    history,
                    *args,
                    "--composition",
                    selected,
                    check=check,
                )

            def submit(
                history: pathlib.Path,
                text: str,
                selected: pathlib.Path = config,
                cwd: pathlib.Path | None = None,
                check: bool = True,
            ) -> subprocess.CompletedProcess[str]:
                args: list[str | pathlib.Path] = [
                    "--composition",
                    selected,
                    "--session",
                    history,
                    "--json",
                ]
                if cwd is not None:
                    args.extend(["--cwd", cwd])
                return command(*args, text, check=check)

            history = scratch / "tree.jsonl"
            events(submit(history, "ROOT_TASK", cwd=project))
            baseline = records(history)
            root = next(
                record["sequence"]
                for record in baseline
                if record["kind"] == "message" and record["payload"].get("role") == "user"
            )
            events(submit(history, "LEFT_BRANCH_SECRET"))
            old = records(history)
            session("enqueue", history, "LEFT_PENDING", "--kind", "follow_up")
            session("branch", history, "--at", str(root), "--branch", "right")
            events(submit(history, "RIGHT_BRANCH_TASK"))
            model_input = json.dumps(server.requests[-1]["input"])
            assert "ROOT_TASK" in model_input and "RIGHT_BRANCH_TASK" in model_input
            assert "LEFT_BRANCH_SECRET" not in model_input and "LEFT_PENDING" not in model_input
            current = records(history)
            assert current[: len(old)] == old
            info = lines(session("info", history))[0]
            assert info["branch"] == "right" and pathlib.Path(info["binding"]["cwd"]).samefile(
                project
            )
            tree = lines(session("tree", history))
            assert any(node["branch"] == "right" and node["parent"] == root for node in tree)
            assert (project / "origin-effect.txt").read_text(
                encoding="utf-8"
            ) == "recorded cwd effect\n"
            results["branch_input"] = {
                "common_ancestor_visible": True,
                "departed_branch_and_queue_absent": True,
                "old_nodes_unchanged": len(old),
                "branch": info["branch"],
            }

            source_hash = digest(history)
            fork, clone = scratch / "fork.jsonl", scratch / "clone.jsonl"
            preview = lines(session("fork", history, fork, "--at", str(root)))[0]
            assert not fork.exists() and digest(history) == source_hash
            session("fork", history, fork, "--at", str(root), "--apply")
            session("clone", history, clone, "--apply")
            fork_records, clone_records = records(fork), records(clone)
            assert len(fork_records) < len(clone_records)
            assert not any(record["kind"].startswith("queue_") for record in clone_records)
            assert len({records(path)[0]["session_id"] for path in [history, fork, clone]}) == 3
            assert digest(history) == source_hash
            events(submit(fork, "FORK_INDEPENDENT_CONTINUATION"))
            assert "LEFT_BRANCH_SECRET" not in json.dumps(server.requests[-1]["input"])
            assert digest(history) == source_hash
            assert session("clone", history, clone, "--apply", check=False).returncode != 0
            results["copies"] = {
                "preview_has_no_destination": True,
                "fresh_session_ids": True,
                "fork_ancestor_only": True,
                "clone_keeps_tree": True,
                "pending_not_copied": True,
                "source_unchanged": source_hash,
                "preview_losses": preview["losses"],
            }

            mismatch = submit(history, "MUST_NOT_RUN_WRONG_CWD", cwd=relocated, check=False)
            assert mismatch.returncode != 0
            moved = scratch / "moved.jsonl"
            session("clone", history, moved, "--cwd", relocated, "--apply")
            events(submit(moved, "MOVED_CONTINUATION"))
            moved_info = lines(session("info", moved))[0]
            assert pathlib.Path(moved_info["binding"]["cwd"]).samefile(relocated)
            assert (relocated / "moved-effect.txt").read_text(
                encoding="utf-8"
            ) == "relocated cwd effect\n"
            assert not (project / "moved-effect.txt").exists()
            assert not (caller / "moved-effect.txt").exists()
            assert digest(history) == source_hash
            hidden_project = scratch / "relocated-offline"
            relocated.rename(hidden_project)
            try:
                assert lines(session("info", moved))
                assert command("history", "inspect", moved).returncode == 0
                assert submit(moved, "MISSING_CWD_MUST_NOT_RUN", check=False).returncode != 0
            finally:
                hidden_project.rename(relocated)
            results["cwd_binding"] = {
                "caller_cwd_ignored_on_reopen": True,
                "conflicting_explicit_cwd_rejected": True,
                "explicit_copy_binds_new_cwd": True,
                "real_tool_effect_only_in_relocated_project": True,
                "missing_cwd_still_readable": True,
            }
            results["stale_preview"] = lines(
                run(
                    [
                        probe,
                        config,
                        project,
                        history,
                        "stale-copy",
                        scratch / "stale.jsonl",
                    ],
                    caller,
                )
            )[0]

            legacy = scratch / "legacy.jsonl"
            legacy_records = copy.deepcopy(baseline)
            for index, record in enumerate(legacy_records):
                record.update(schema_version=1, sequence=index + 1)
                record.pop("parent_id", None)
                record.pop("branch", None)
            legacy.write_text(
                "".join(json.dumps(record) + "\n" for record in legacy_records),
                encoding="utf-8",
            )
            legacy_hash = digest(legacy)
            inspected = command("history", "inspect", legacy)
            assert len(lines(inspected)) == len(legacy_records)
            before = len(server.requests)
            assert submit(legacy, "REJECT_V1_WRITER", check=False).returncode != 0
            assert len(server.requests) == before and digest(legacy) == legacy_hash
            upgraded = scratch / "upgraded.jsonl"
            session("upgrade", legacy, upgraded)
            assert not upgraded.exists()
            session("upgrade", legacy, upgraded, "--apply")
            assert all(record["schema_version"] == 2 for record in records(upgraded))
            events(submit(upgraded, "UPGRADED_CONTINUATION"))
            assert digest(legacy) == legacy_hash
            damaged = scratch / "damaged.jsonl"
            damaged.write_bytes(history.read_bytes() + b'{"schema_version":2,"transaction":[')
            damaged_hash = digest(damaged)
            prefix = command("history", "inspect", damaged, check=False)
            assert prefix.returncode == 2 and "incomplete" in prefix.stderr
            assert len(lines(prefix)) == len(records(history))
            recovered = scratch / "recovered.jsonl"
            session("recover", damaged, recovered)
            assert not recovered.exists() and digest(damaged) == damaged_hash
            session("recover", damaged, recovered, "--apply")
            events(submit(recovered, "RECOVERED_CONTINUATION"))
            assert digest(damaged) == damaged_hash
            results["upgrade_recovery"] = {
                "legacy_readable_but_not_writable": True,
                "legacy_source_sha256": legacy_hash,
                "damaged_prefix_records": len(lines(prefix)),
                "damage_source_sha256": damaged_hash,
                "reopened_copies_completed": True,
            }

            unavailable = destination / "plugins-unavailable"
            (destination / "plugins").rename(unavailable)
            try:
                exported = scratch / "exported.jsonl"
                assert lines(command("history", "inspect", history))
                command("history", "export", history, exported)
                assert records(exported) == records(history)
                assert command("history", "export", history, exported, check=False).returncode != 0
            finally:
                unavailable.rename(destination / "plugins")
            results["offline_public_history"] = {
                "all_native_libraries_unavailable": True,
                "inspect_and_export_succeeded": True,
                "export_records": len(records(exported)),
                "existing_destination_rejected": True,
            }

            selected = copy.deepcopy(composition)
            selected["packages"].append(author)
            selected["roles"].update(
                {
                    CONTEXT: "coding-replacements",
                    STORE: "coding-replacements",
                    INTERPRETER: "coding-replacements",
                    MIGRATOR: "coding-replacements",
                }
            )
            independent = configure(destination, selected, server, "independent-sessions")
            state_history = scratch / "extension-v1.jsonl"
            seed = lines(run([probe, independent, project, state_history, "state", "1"], caller))[0]
            events(submit(state_history, "INTERPRET_REQUIRED_COUNTER", selected=independent))
            assert "AUTHOR_STATE=17" in json.dumps(server.requests[-1]["input"])
            assert "INDEPENDENT_CONTEXT_MARKER" in json.dumps(server.requests[-1]["input"])
            raw_before = records(state_history)
            session("compact", state_history, selected=independent)
            compacted = records(state_history)
            assert compacted[: len(raw_before)] == raw_before
            assert any(
                record["kind"] == "compaction"
                and record["payload"].get("author") == "coding-replacements"
                for record in compacted
            )
            events(submit(state_history, "AFTER_INDEPENDENT_COMPACTION", selected=independent))
            assert "INDEPENDENT_SUMMARY" in json.dumps(server.requests[-1]["input"])
            assert "AUTHOR_STATE=17" in json.dumps(server.requests[-1]["input"])
            before_branch = records(state_history)
            session(
                "branch",
                state_history,
                "--at",
                "1",
                "--branch",
                "author-carried",
                "--summarize",
                selected=independent,
            )
            events(submit(state_history, "AFTER_AUTHOR_BRANCH_SUMMARY", selected=independent))
            assert records(state_history)[: len(before_branch)] == before_branch
            carried = [
                record for record in records(state_history) if record["kind"] == "branch_summary"
            ]
            assert carried and carried[-1]["payload"]["origin_ids"]
            assert "INDEPENDENT_SUMMARY" in json.dumps(server.requests[-1]["input"])
            results["independent_context_and_store"] = {
                "sdk_only_author": True,
                "seed": seed,
                "required_state_observed_in_model_input": True,
                "summary_observed_after_reopen": True,
                "raw_records_preserved": len(raw_before),
                "detached_branch_summary_visible_after_reopen": True,
            }

            requests_before_pipe_failure = len(server.requests)
            results["broken_stdout_cleanup"] = [
                broken_stdout(
                    host,
                    selected,
                    destination,
                    server,
                    state_history,
                    caller,
                    verb,
                    arguments,
                )
                for verb, arguments in [
                    ("queue", []),
                    ("enqueue", ["BROKEN_PIPE_QUEUE"]),
                    ("metadata", ["--name", "saved before output failure"]),
                ]
            ]
            assert len(server.requests) == requests_before_pipe_failure
            pipe_records = records(state_history)
            assert any(
                record["kind"] == "queue_accepted"
                and "BROKEN_PIPE_QUEUE" in json.dumps(record["payload"])
                for record in pipe_records
            )
            assert any(
                record["kind"] == "session_metadata"
                and record["payload"]["name"] == "saved before output failure"
                for record in pipe_records
            )

            unknown = scratch / "unknown-required.jsonl"
            run(
                [probe, independent, project, unknown, "state", "1", "unknown.counter"],
                caller,
            )
            before = len(server.requests)
            assert (
                submit(unknown, "MUST_NOT_CALL_MODEL", selected=independent, check=False).returncode
                != 0
            )
            assert len(server.requests) == before
            missing = copy.deepcopy(selected)
            missing["roles"].pop(INTERPRETER)
            missing_config = configure(destination, missing, server, "missing-interpreter")
            missing_history = scratch / "missing-interpreter.jsonl"
            run([probe, missing_config, project, missing_history, "state", "1"], caller)
            assert (
                submit(
                    missing_history,
                    "MISSING_INTERPRETER_NO_MODEL",
                    selected=missing_config,
                    check=False,
                ).returncode
                != 0
            )
            assert len(server.requests) == before
            results["required_state_guard"] = {
                "unknown_namespace_blocks_model": True,
                "missing_interpreter_blocks_model": True,
            }

            old_state = scratch / "extension-v0.jsonl"
            run([probe, independent, project, old_state, "state", "0"], caller)
            old_hash = digest(old_state)
            migrated = scratch / "migrated.jsonl"
            plan = lines(session("migrate", old_state, migrated, selected=independent))[0]
            assert not migrated.exists() and digest(old_state) == old_hash
            session("migrate", old_state, migrated, "--apply", selected=independent)
            migrated_state = next(
                record["payload"]
                for record in records(migrated)
                if record["kind"] == "extension_state"
            )
            assert migrated_state["version"] == 1 and migrated_state["value"] == {
                "count": 17,
                "label": "preserved",
            }
            events(submit(migrated, "MIGRATED_COUNTER", selected=independent))
            assert "AUTHOR_STATE=17" in json.dumps(server.requests[-1]["input"])
            assert digest(old_state) == old_hash
            failing = copy.deepcopy(selected)
            failing["packages"][-1]["config"] = {"fail_migration": True}
            fail_config = configure(destination, failing, server, "failing-migration")
            fail_destination = scratch / "failed-migration.jsonl"
            assert (
                session(
                    "migrate",
                    old_state,
                    fail_destination,
                    "--apply",
                    selected=fail_config,
                    check=False,
                ).returncode
                != 0
            )
            assert not fail_destination.exists() and digest(old_state) == old_hash
            false_author = copy.deepcopy(selected)
            false_author["packages"][-1]["config"] = {"false_claim": True}
            false_config = configure(destination, false_author, server, "false-migration")
            false_destination = scratch / "false-migration.jsonl"
            assert (
                session(
                    "migrate",
                    old_state,
                    false_destination,
                    "--apply",
                    selected=false_config,
                    check=False,
                ).returncode
                != 0
            )
            assert not false_destination.exists() and digest(old_state) == old_hash
            results["independent_migration"] = {
                "preview_preserved": plan["preserved"],
                "actual_v0_to_v1_conversion": migrated_state,
                "interpreter_confirmed_after_reopen": True,
                "failure_leaves_destination_absent": True,
                "false_conversion_claim_rejected": True,
                "source_sha256": old_hash,
            }

            memory = scratch / "memory"
            memory.mkdir()
            events(
                command(
                    "--composition",
                    config,
                    "--cwd",
                    memory,
                    "--no-session",
                    "--json",
                    "MEMORY_SUCCESS",
                )
            )
            assert list(memory.iterdir()) == []
            bad_memory = copy.deepcopy(selected)
            bad_memory["packages"][-1]["config"] = {"fail_kind": "message"}
            bad_memory_config = configure(destination, bad_memory, server, "bad-memory")
            before = len(server.requests)
            assert (
                command(
                    "--composition",
                    bad_memory_config,
                    "--cwd",
                    memory,
                    "--no-session",
                    "--json",
                    "MEMORY_FAILURE",
                    check=False,
                ).returncode
                != 0
            )
            assert len(server.requests) == before and list(memory.iterdir()) == []
            results["memory"] = {
                "success_and_persistence_failure_create_no_files": True,
                "failed_commit_prevents_model_call": True,
            }
        finally:
            server.close()
        assert digest(host) == host_hash
    evidence = {
        "platform": platform.platform(),
        "target": target(),
        "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(),
        "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()),
        "host_sha256": host_hash,
        "host_unchanged_after_external_author_build": True,
        "provider_evidence": "controlled Responses HTTP",
        "scenarios": results,
    }
    write(artifacts / "session-verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
