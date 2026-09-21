#!/usr/bin/env python3
"""Installed coding acceptance with controlled HTTP and independent SDK authors."""

import copy
import hashlib
import http.server
import json
import os
import pathlib
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
from collections.abc import Callable
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT, Composition, Package, library, package, target
from verification import author_artifact, example, installed, prepare, run, short_retries

PROVIDER = "eden.coding-provider.v1"
CONTEXT = "eden.coding-context.v2"
TOOL = "eden.coding-tool.v1"
STORE = "eden.session-store.v2"


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def records(path: pathlib.Path) -> list[dict[str, Any]]:
    return [
        record
        for line in path.read_text(encoding="utf-8").splitlines()
        for record in (lambda value: value.get("transaction", [value]))(json.loads(line))
    ]


def answer(text: str) -> list[dict[str, Any]]:
    return [
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }
    ]


def call(identity: str, name: str, **arguments: Any) -> list[dict[str, Any]]:
    return [
        {
            "type": "function_call",
            "call_id": identity,
            "name": name,
            "arguments": json.dumps(arguments),
        }
    ]


class Server:
    def __init__(
        self,
        respond: Callable[[dict[str, Any], int], list[dict[str, Any]]],
        gated: bool = False,
        partial: bool = False,
    ) -> None:
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                owner.release.set()
                self.send_response(200)
                self.end_headers()

            def do_POST(self) -> None:
                try:
                    assert self.headers["Authorization"] == "Bearer controlled-verifier-key"
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    assert (
                        body["model"] == "controlled-model"
                        and body["stream"] is True
                        and body["store"] is False
                    )
                    owner.requests.append(body)
                    output = respond(body, len(owner.requests) - 1)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(
                        b'data: {"type":"response.output_text.delta","delta":"provider gate"}\n\n'
                    )
                    self.wfile.flush()
                    if partial:
                        return
                    if gated and len(owner.requests) == 1:
                        assert owner.release.wait(30), "provider gate was not released"
                    event = {
                        "type": "response.completed",
                        "response": {
                            "status": "completed",
                            "output": output,
                            "usage": {"input_tokens": 10, "output_tokens": 5},
                        },
                    }
                    self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except BaseException as error:
                    owner.errors.append(repr(error))

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.address = f"127.0.0.1:{self.http.server_port}"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.release.set()
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


def configure(
    destination: pathlib.Path, composition: Composition, server: Server, name: str
) -> pathlib.Path:
    selected = copy.deepcopy(composition)
    for item in selected["packages"]:
        if item["descriptor"]["package"] == "model-access":
            item["config"] = {
                "endpoint": f"http://{server.address}/v1/responses",
                "model": "controlled-model",
                "api_key_env": "EDEN_VERIFY_KEY",
            }
    # The product doubles a 2 s retry delay per attempt. The suites assert which
    # retries happen, never how long they sleep, so they must not sleep.
    short_retries(selected)
    path = destination / f"{name}.json"
    write(path, selected)
    return path


def events(
    result: subprocess.CompletedProcess[str], completed: bool = True
) -> list[dict[str, Any]]:
    data = [json.loads(line) for line in result.stdout.splitlines()]
    accepted = next(i for i, item in enumerate(data) if item["kind"] == "accepted")
    assert data[-1]["kind"] == "settled" and accepted < len(data) - 1
    terminal = data[-1]["payload"]
    assert (terminal["outcome"]["status"] == "completed") is completed, terminal
    assert terminal["cleanup_errors"] == [], terminal
    assert [e["sequence"] for e in data] == list(
        range(data[0]["sequence"], data[-1]["sequence"] + 1)
    )
    assert (result.returncode == 0) is completed
    return data


def build_author(destination: pathlib.Path, scratch: pathlib.Path) -> Package:
    lib = library("author_coding_replacements")
    folder = destination / "plugins/coding-replacements/0.1.0"
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy2(author_artifact("coding-replacements"), folder / lib)
    return package(
        "coding-replacements",
        [
            PROVIDER,
            CONTEXT,
            TOOL,
            STORE,
            "eden.record-interpreter.v1",
            "eden.state-migrator.v1",
        ],
        f"plugins/coding-replacements/0.1.0/{lib}",
        target(),
    )


def main() -> None:
    os.environ["EDEN_VERIFY_KEY"] = "controlled-verifier-key"
    prepare()
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = installed(artifacts / "coding-install")
    suffix = ".exe" if sys.platform == "win32" else ""
    host = destination / "bin" / ("eden" + suffix)
    probe = example("coding_probe")
    fixed_host = host.read_bytes()
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-coding-acceptance-") as temp:
        scratch = pathlib.Path(temp)
        os.environ["EDEN_AGENT_DIR"] = str(scratch / "global")
        (scratch / "global").mkdir()
        write(
            scratch / "global/settings.json",
            {"discover_skills": False, "discover_templates": False},
        )
        author = build_author(destination, scratch)
        caller = scratch / "unrelated caller 工作目录"
        caller.mkdir()
        project = scratch / "coding project"
        project.mkdir()
        # The installed read result must survive history and reach the next real
        # HTTP request as an image block, rather than base64 inside display text.
        import base64

        image_bytes = base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII="
        )
        (project / "pixel.png").write_bytes(image_bytes)
        parity_history = scratch / "tool-parity.jsonl"
        (project / "atomic.txt").write_bytes(b"\xef\xbb\xbfalpha\r\nbeta\r\n")
        parity_calls = [
            call("image-read", "read", path="pixel.png"),
            call(
                "batch-invalid",
                "edit",
                path="atomic.txt",
                edits=[
                    {"old_text": "alpha", "new_text": "A"},
                    {"old_text": "missing", "new_text": "B"},
                ],
            ),
            call(
                "batch-valid",
                "edit",
                path="atomic.txt",
                edits=[
                    {"old_text": "alpha", "new_text": "A"},
                    {"old_text": "beta", "new_text": "B"},
                ],
            ),
            call("range-read", "read", path="atomic.txt", limit=1),
        ]

        def parity_response(body: dict[str, Any], index: int) -> list[dict[str, Any]]:
            if index:
                output = [item for item in body["input"] if item["type"] == "function_call_output"][
                    -1
                ]["output"]
                if index == 1:
                    assert isinstance(output, list)
                    image = next(block for block in output if block["type"] == "input_image")
                    assert base64.b64decode(image["image_url"].split(",", 1)[1]) == image_bytes
                    stored = next(
                        r["payload"]["result"]
                        for r in records(parity_history)
                        if r["kind"] == "tool_result"
                    )
                    assert base64.b64decode(stored["content"][0]["data"]) == image_bytes
                else:
                    result = json.loads(output)
                    if index == 2:
                        assert result["error"] is not None
                        assert (
                            project / "atomic.txt"
                        ).read_bytes() == b"\xef\xbb\xbfalpha\r\nbeta\r\n"
                    elif index == 3:
                        assert result["error"] is None
                        assert (project / "atomic.txt").read_bytes() == b"\xef\xbb\xbfA\r\nB\r\n"
                    elif index == 4:
                        assert result["details"]["next_offset"] == 2
            return parity_calls[index] if index < len(parity_calls) else answer("parity complete")

        parity_server = Server(parity_response)
        try:
            parity_config = configure(destination, composition, parity_server, "tool-parity")
            events(
                run(
                    [
                        host,
                        "--composition",
                        parity_config,
                        "--cwd",
                        project,
                        "--global-dir",
                        scratch / "global",
                        "--session",
                        parity_history,
                        "--json",
                        "Inspect image and perform validated file operations",
                    ],
                    caller,
                )
            )
            assert len(parity_server.requests) == 5
            results["tool_result_parity"] = {
                "requests": 5,
                "image_bytes_in_wire_and_history": len(image_bytes),
                "batch_validation_preserves_original": True,
                "bom_crlf_preserved": True,
                "next_offset": 2,
            }
        finally:
            parity_server.close()

        skill_path = scratch / "frozen-skill.md"
        skill_path.write_text(
            "---\nname: frozen\ndescription: test frozen instructions\n---\nBODY", encoding="utf-8"
        )
        system_path = scratch / "global/SYSTEM.md"
        system_path.write_text("CUSTOM-ROLE-PARITY", encoding="utf-8")
        for selected_tools in [["skill"], ["read"], []]:

            def inspect_prompt(
                body: dict[str, Any], _index: int, selected_tools: list[str] = selected_tools
            ) -> list[dict[str, Any]]:
                prompt = "\n".join(
                    block["text"]
                    for item in body["input"]
                    if item.get("role") == "system"
                    for block in item["content"]
                    if block["type"] == "input_text"
                )
                actual_cwd = next(
                    line.removeprefix("Working directory: ")
                    for line in prompt.splitlines()
                    if line.startswith("Working directory: ")
                )
                assert "CUSTOM-ROLE-PARITY" in prompt
                assert pathlib.Path(actual_cwd).samefile(project), (actual_cwd, str(project))
                assert ("Use the skill tool" in prompt) is (selected_tools == ["skill"])
                assert ("eden-resource://skill/frozen" in prompt) is (selected_tools == ["read"])
                assert ("Available skill frozen" in prompt) is bool(selected_tools)
                return answer("prompt consistent")

            prompt_server = Server(inspect_prompt)
            try:
                selected = copy.deepcopy(composition)
                for entry in selected["packages"]:
                    if entry["descriptor"]["package"] == "workspace-resources":
                        entry["config"] = {"skill_paths": [str(skill_path)]}
                    if entry["descriptor"]["package"] == "coding-tools":
                        entry["config"] = {"tools": selected_tools}
                prompt_config = configure(destination, selected, prompt_server, "prompt-parity")
                events(
                    run(
                        [
                            host,
                            "--composition",
                            prompt_config,
                            "--cwd",
                            project,
                            "--global-dir",
                            scratch / "global",
                            "--session",
                            scratch / f"prompt-{len(selected_tools)}-{selected_tools}.jsonl",
                            "--json",
                            "Describe available skills",
                        ],
                        caller,
                    )
                )
                assert len(prompt_server.requests) == 1
            finally:
                prompt_server.close()
        system_path.unlink()
        results["effective_skill_prompts"] = {
            "default_skill": True,
            "read_only_frozen_uri": True,
            "no_reader_no_advertisement": True,
            "custom_system_keeps_cwd": True,
        }

        source = project / "arithmetic.py"
        original = "# TODO TODO\ndef add(a, b):\n    return a - b\n"
        source.write_text(original, encoding="utf-8")
        history = scratch / "coding.jsonl"
        test_source = f"""import json, pathlib, unittest
from arithmetic import add
class ArithmeticTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)
        self.assertEqual(add(-2, 3), 1)
        history = [record for x in pathlib.Path({str(history)!r}).read_text(encoding="utf-8").splitlines() for record in json.loads(x).get("transaction", [json.loads(x)])]
        self.assertTrue(any(x["kind"] == "tool_intent" and x["payload"].get("call_id") == "tests" for x in history))
        pathlib.Path("tests-ran.txt").write_text("passed after durable intent", encoding="utf-8")
"""
        python = shlex.quote(pathlib.Path(sys.executable).as_posix())
        commands = [
            call("read", "read", path="arithmetic.py"),
            call(
                "absent",
                "edit",
                path="arithmetic.py",
                old_text="not present",
                new_text="bad",
            ),
            call(
                "ambiguous",
                "edit",
                path="arithmetic.py",
                old_text="TODO",
                new_text="bad",
            ),
            call(
                "repair",
                "edit",
                path="arithmetic.py",
                old_text="return a - b",
                new_text="return a + b",
            ),
            call("write-tests", "write", path="test_arithmetic.py", content=test_source),
            call(
                "tests",
                "bash",
                command=f"{python} -m unittest -v && {python} -c 'print(\"x\" * 100000)'",
            ),
            call("nonzero", "bash", command="exit 7"),
        ]

        def coding(body: dict[str, Any], index: int) -> list[dict[str, Any]]:
            if index:
                result = json.loads(
                    [item for item in body["input"] if item["type"] == "function_call_output"][-1][
                        "output"
                    ]
                )
                previous = commands[index - 1][0]["call_id"]
                persisted = records(history)
                assert any(
                    r["kind"] == "tool_result" and r["payload"]["call_id"] == previous
                    for r in persisted
                )
                if index == 1:
                    assert result["error"] is None and "return a - b" in result["text"]
                if index in [2, 3]:
                    assert (
                        result["error"] is not None
                        and source.read_text(encoding="utf-8") == original
                    )
                if index == 4:
                    assert result["error"] is None and source.read_text(
                        encoding="utf-8"
                    ) == original.replace("return a - b", "return a + b")
                if index == 5:
                    assert (
                        result["error"] is None
                        and (project / "test_arithmetic.py").read_text(encoding="utf-8")
                        == test_source
                    )
                if index == 6:
                    assert result["exit_code"] == 0 and result["truncated"] is True
                    streams = {a["name"]: pathlib.Path(a["path"]) for a in result["artifacts"]}
                    assert streams["stdout"].read_bytes() == b"x" * 100000 + os.linesep.encode()
                    assert b"Ran 1 test" in streams["stderr"].read_bytes()
                    assert "Ran 1 test" in result["text"]
                    assert (project / "tests-ran.txt").read_text(
                        encoding="utf-8"
                    ) == "passed after durable intent"
                if index == 7:
                    assert result["exit_code"] == 7
            return (
                commands[index]
                if index < len(commands)
                else answer(
                    "Repaired addition; two cases passed. Long output was truncated and exit 7 was observed."
                )
            )

        server = Server(coding)
        config = configure(destination, composition, server, "coding-task")
        try:
            data = events(
                run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--session",
                        history,
                        "--json",
                        "Repair addition and verify with tests",
                    ],
                    caller,
                )
            )
            assert len(server.requests) == 8
            assert "return a + b" in source.read_text(encoding="utf-8")
            results["coding_task"] = {
                "requests": len(server.requests),
                "terminal": data[-1]["payload"],
                "tools": [c[0]["name"] for c in commands],
                "intent_observed_by_test_process": True,
                "exact_edit_failure_preserved_source": True,
                "long_output_truncated": True,
                "nonzero_exit": 7,
            }
        finally:
            server.close()
        before = records(history)
        resume = Server(
            lambda body, _: answer(
                "Reopened history includes the repaired addition and test results."
            )
        )
        # Endpoint config is not part of the durable role identity.
        config = configure(destination, composition, resume, "coding-reopen")
        try:
            events(
                run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--session",
                        history,
                        "--json",
                        "Review the previous coding result",
                    ],
                    caller,
                )
            )
            assert any(
                i["type"] == "function_call_output" and i["call_id"] == "tests"
                for i in resume.requests[0]["input"]
            )
            after = records(history)
            assert after[: len(before)] == before and after[-1]["run_id"] > before[-1]["run_id"]
            assert len({r["session_id"] for r in after}) == 1
            results["reopen"] = {
                "preserved_records": len(before),
                "new_record_count": len(after),
                "test_result_in_model_input": True,
            }
        finally:
            resume.close()

        partial = Server(lambda *_: [], partial=True)
        try:
            config = configure(destination, composition, partial, "partial")
            failed = events(
                run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--session",
                        scratch / "partial.jsonl",
                        "--json",
                        "partial stream",
                    ],
                    caller,
                    check=False,
                ),
                completed=False,
            )
            assert any(e["kind"] == "model_text_delta" for e in failed)
            results["partial_stream"] = failed[-1]["payload"]
        finally:
            partial.close()

        # Explicit env-file is a CLI startup input; no implicit project/parent discovery.
        env_server = Server(lambda *_: answer("explicit env-file request"))
        try:
            env_project = scratch / "env parent"
            env_caller = env_project / "child"
            env_caller.mkdir(parents=True)
            env_file = env_project / ".env"
            env_file.write_text(
                f'OPENAI_MODEL="file-model"\nOPENAI_BASE_URL=http://{env_server.address}/v1\nOPENAI_API_KEY=wrong-file-key\n',
                encoding="utf-8",
            )
            env_config = destination / "env-only.json"
            write(env_config, composition)
            isolated = dict(os.environ)
            for key in [
                "OPENAI_MODEL",
                "OPENAI_BASE_URL",
                "OPENAI_API_KEY",
                "EDEN_RESPONSES_PROFILE",
                "EDEN_API_KEY_ENV",
                "OPENAI_MAX_OUTPUT_TOKENS",
                "OPENAI_REASONING_EFFORT",
            ]:
                isolated.pop(key, None)
            missing = run(
                [
                    host,
                    "--composition",
                    env_config,
                    "--cwd",
                    env_caller,
                    "--no-session",
                    "--json",
                    "no implicit env",
                ],
                env_caller,
                check=False,
                env=isolated,
            )
            assert missing.returncode != 0 and not env_server.requests
            inherited = dict(
                isolated,
                OPENAI_MODEL="controlled-model",
                OPENAI_API_KEY="controlled-verifier-key",
            )
            explicit = run(
                [
                    host,
                    "--env-file",
                    "../.env",
                    "--composition",
                    env_config,
                    "--cwd",
                    project,
                    "--no-session",
                    "--json",
                    "explicit env",
                ],
                env_caller,
                env=inherited,
            )
            events(explicit)
            assert len(env_server.requests) == 1
            # Case aliases must retain source order on case-insensitive Windows environments.
            case_file = env_project / "case.env"
            case_file.write_text(
                f"openai_model=wrong-case-model\nOPENAI_MODEL=controlled-model\nOPENAI_BASE_URL=http://{env_server.address}/v1\nOPENAI_API_KEY=controlled-verifier-key\n",
                encoding="utf-8",
            )
            events(
                run(
                    [
                        host,
                        "--env-file",
                        "../case.env",
                        "--composition",
                        env_config,
                        "--cwd",
                        project,
                        "--no-session",
                        "--json",
                        "case precedence",
                    ],
                    env_caller,
                    env=isolated,
                )
            )
            assert len(env_server.requests) == 2
            # Original explicit package settings also remain stronger than environment values.
            preferred = configure(destination, composition, env_server, "package-precedence")
            overridden = dict(
                inherited,
                OPENAI_MODEL="wrong-process-model",
                OPENAI_BASE_URL="http://127.0.0.1:1",
            )
            events(
                run(
                    [
                        host,
                        "--env-file",
                        "../.env",
                        "--composition",
                        preferred,
                        "--cwd",
                        project,
                        "--no-session",
                        "--json",
                        "package wins",
                    ],
                    env_caller,
                    env=overridden,
                )
            )
            assert len(env_server.requests) == 3
            results["explicit_env_file"] = {
                "parent_discovery": False,
                "relative_to_caller": True,
                "environment_over_file": True,
                "package_over_environment": True,
                "case_alias_assignment_order": True,
            }
        finally:
            env_server.close()

        for role, label in [
            (PROVIDER, "provider"),
            (CONTEXT, "context"),
            (TOOL, "tool"),
            (STORE, "store"),
        ]:
            selected = copy.deepcopy(composition)
            selected["packages"].append(author)
            selected["roles"][role] = "coding-replacements"
            replacement_project = scratch / f"replacement-{label}"
            replacement_project.mkdir()
            replacement_history = scratch / f"replacement-{label}.jsonl"

            def replacement(
                body: dict[str, Any], index: int, label: str = label
            ) -> list[dict[str, Any]]:
                if label == "context":
                    assert "INDEPENDENT_CONTEXT_MARKER" in json.dumps(body["input"])
                    assert [t["name"] for t in body["tools"]] == ["write"]
                if index == 0:
                    return call(
                        f"{label}-write",
                        "write",
                        path="model-created.txt",
                        content="changed by controlled provider\n",
                    )
                result = json.loads(
                    next(
                        item["output"]
                        for item in body["input"]
                        if item["type"] == "function_call_output"
                    )
                )
                if label == "tool":
                    assert result["text"] == "INDEPENDENT_TOOL_RESULT"
                return answer("Final model input contained " + result["text"])

            server = Server(replacement)
            try:
                config = configure(destination, selected, server, f"replacement-{label}")
                data = events(
                    run(
                        [
                            host,
                            "--composition",
                            config,
                            "--cwd",
                            replacement_project,
                            "--session",
                            replacement_history,
                            "--json",
                            "write a file",
                        ],
                        caller,
                    )
                )
                if label == "provider":
                    assert not server.requests
                    assert (replacement_project / "author-created.txt").read_text(
                        encoding="utf-8"
                    ) == "independent provider wrote this\n"
                elif label == "tool":
                    assert (replacement_project / "author-tool-effect.txt").read_text(
                        encoding="utf-8"
                    ) == "write:tool-write"
                    assert "INDEPENDENT_TOOL_RESULT" in json.dumps(data[-1])
                else:
                    assert (replacement_project / "model-created.txt").read_text(
                        encoding="utf-8"
                    ) == "changed by controlled provider\n"
                recovery = None
                if label == "store":
                    prior = records(replacement_history)
                    resumed = events(
                        run(
                            [
                                host,
                                "--composition",
                                config,
                                "--cwd",
                                replacement_project,
                                "--session",
                                replacement_history,
                                "--json",
                                "Review the preserved tool result",
                            ],
                            caller,
                        )
                    )
                    recovered = records(replacement_history)
                    assert recovered[: len(prior)] == prior
                    assert recovered[-1]["run_id"] > prior[-1]["run_id"]
                    assert len({r["session_id"] for r in recovered}) == 1
                    assert any(
                        i["type"] == "function_call_output" and i["call_id"] == "store-write"
                        for i in server.requests[-1]["input"]
                    )
                    recovery = {
                        "preserved_records": len(prior),
                        "new_run_id": recovered[-1]["run_id"],
                        "terminal": resumed[-1]["payload"],
                    }
                results[f"independent_{label}"] = {
                    "terminal": data[-1]["payload"],
                    "selected_role": role,
                }
                if recovery:
                    results[f"independent_{label}"]["recovery"] = recovery
            finally:
                server.close()

        for fail_kind in ["tool_intent", "tool_result"]:
            selected = copy.deepcopy(composition)
            failing_author = copy.deepcopy(author)
            failing_author["config"] = {"fail_kind": fail_kind}
            selected["packages"].append(failing_author)
            selected["roles"][STORE] = "coding-replacements"
            failure_project = scratch / f"fail-{fail_kind}"
            failure_project.mkdir()
            failure_history = scratch / f"fail-{fail_kind}.jsonl"
            call_id = f"failure-{fail_kind}"
            server = Server(
                lambda *_, call_id=call_id: call(
                    call_id,
                    "write",
                    path="effect.txt",
                    content="real effect before result persistence\n",
                )
            )
            try:
                config = configure(destination, selected, server, f"fail-{fail_kind}")
                data = events(
                    run(
                        [
                            host,
                            "--composition",
                            config,
                            "--cwd",
                            failure_project,
                            "--session",
                            failure_history,
                            "--json",
                            "perform the write",
                        ],
                        caller,
                        check=False,
                    ),
                    completed=False,
                )
                terminal = data[-1]["payload"]
                assert terminal["outcome"]["value"]["code"] == "PersistenceFailure", terminal
                assert len(server.requests) == 1
                durable = records(failure_history)
                assert not any(r["kind"] == fail_kind for r in durable)
                if fail_kind == "tool_intent":
                    assert not (failure_project / "effect.txt").exists()
                else:
                    assert (failure_project / "effect.txt").read_text(
                        encoding="utf-8"
                    ) == "real effect before result persistence\n"
                    assert call_id in terminal["outcome"]["value"]["message"], terminal
                    assert "may already have occurred" in terminal["outcome"]["value"]["message"], (
                        terminal
                    )
                    assert any(
                        r["kind"] == "tool_intent" and r["payload"]["call_id"] == call_id
                        for r in durable
                    )
                results[f"persistence_failure_{fail_kind}"] = {
                    "terminal": terminal,
                    "effect_occurred": (failure_project / "effect.txt").exists(),
                    "public_records": len(durable),
                }
            finally:
                server.close()

        # An interrupted durable intention is context, never executable work on
        # reopen: the run must not perform the write it names, and nothing may
        # appear on disk for it.
        # The fabricated history must carry the binding this scenario reopens
        # with (cwd, roles and package descriptors), so a plain session in the
        # same project seeds it.
        seed_history = scratch / "reopen-seed.jsonl"
        seeding = Server(lambda *_: answer("seeded session for the interrupted reopen"))
        try:
            seeding_config = configure(destination, composition, seeding, "reopen-seed")
            events(
                run(
                    [
                        host,
                        "--composition",
                        seeding_config,
                        "--cwd",
                        project,
                        "--session",
                        seed_history,
                        "--json",
                        "Seed the durable history",
                    ],
                    caller,
                )
            )
        finally:
            seeding.close()
        interrupted = scratch / "interrupted.jsonl"
        seed = copy.deepcopy(records(seed_history)[:1])
        seed.append(
            {
                "schema_version": 2,
                "parent_id": 1,
                "branch": "main",
                "session_id": seed[0]["session_id"],
                "sequence": 2,
                "run_id": 1,
                "kind": "tool_intent",
                "payload": {
                    "type": "tool_call",
                    "call_id": "interrupted-write",
                    "name": "write",
                    "arguments": json.dumps({"path": "must-not-exist.txt", "content": "replayed"}),
                },
            }
        )
        interrupted.write_text(
            json.dumps({"schema_version": 2, "transaction": seed}) + "\n", encoding="utf-8"
        )
        server = Server(lambda *_: answer("Historical effects remain unknown."))
        try:
            config = configure(destination, composition, server, "interrupted")
            events(
                run(
                    [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--session",
                        interrupted,
                        "--json",
                        "Review interrupted history",
                    ],
                    caller,
                )
            )
            assert not (project / "must-not-exist.txt").exists()
            assert "was not replayed" in json.dumps(server.requests[0]["input"])
            results["interrupted_intent"] = "reopened as uncertainty without replay"
        finally:
            server.close()

        selected = copy.deepcopy(composition)
        selected["packages"].append(author)
        selected["roles"][STORE] = "coding-replacements"
        config = destination / "open-abandon.json"
        write(config, selected)
        results["abandoned_session_open"] = json.loads(
            run(
                [
                    probe,
                    config,
                    project,
                    scratch / "open-abandon.jsonl",
                    "open-abandon",
                ],
                caller,
            ).stdout
        )

        assert host.read_bytes() == fixed_host
    evidence = {
        "platform": platform.platform(),
        "target": target(),
        "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(),
        "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()),
        "host_unchanged": True,
        "host_sha256": hashlib.sha256(fixed_host).hexdigest(),
        "independent_sdk_only_author_build": True,
        "provider_evidence": "controlled Responses HTTP; real authenticated provider call remains unaccepted",
        "scenarios": results,
    }
    write(artifacts / "coding-verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
