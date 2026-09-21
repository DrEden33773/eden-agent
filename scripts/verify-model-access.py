#!/usr/bin/env python3
"""Installed G1 proof: private credentials, replaceable catalog, and wire replay."""

import copy
import http.server
import json
import os
import pathlib
import shutil
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT, library, package, target
from verification import author_artifact, installed, prepare

CATALOG = "eden.model-catalog.v1"
CREDENTIALS = "eden.credential-source.v1"
MANAGER = "eden.model-manager.v1"
PROVIDER = "eden.coding-provider.v1"
SECRET = "G1-PRIVATE-CANARY-stdin-key"
AUTHOR_SECRET = "G1-PRIVATE-CANARY-independent-key"
MARKER = "tool-result-survives-all-three-wires"


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def model(api: str, base: str) -> dict[str, Any]:
    return {
        "provider": "fixture",
        "model": api,
        "api": api,
        "base_url": base,
        "headers": {"x-author-route": "receiver-proof"},
        "limits": {"context_window": 65536, "max_output_tokens": 777},
        "capabilities": {"tools": True, "images": False, "reasoning": False},
        "source": {"kind": "author", "location": "independent SDK author"},
    }


class Server:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.secret = SECRET
        self.tool_next = True
        self.mutate: pathlib.Path | None = None
        self.catalog_gets = 0
        self.pending_tool: str | None = None
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                owner.catalog_gets += 1
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b"[]")

            def do_POST(self) -> None:
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    api = body["model"]
                    assert api in APIS, body
                    assert self.path == "/v1/" + APIS[api][0], self.path
                    assert self.headers["x-author-route"] == "receiver-proof"
                    key = (
                        self.headers.get("x-api-key")
                        if api == "anthropic-messages"
                        else self.headers.get("Authorization", "").removeprefix("Bearer ")
                    )
                    assert key == owner.secret
                    assert body[APIS[api][1]] == 777, body
                    owner.requests.append(body)
                    tool = owner.tool_next
                    owner.tool_next = False
                    if not tool:
                        assert MARKER in json.dumps(body), body
                    if owner.pending_tool is not None:
                        if api == "openai-responses":
                            result = next(
                                item["output"]
                                for item in body["input"]
                                if item.get("type") == "function_call_output"
                                and item.get("call_id") == owner.pending_tool
                            )
                        elif api == "openai-completions":
                            result = next(
                                item["content"]
                                for item in body["messages"]
                                if item.get("role") == "tool"
                                and item.get("tool_call_id") == owner.pending_tool
                            )
                        else:
                            result = next(
                                block["content"]
                                for item in body["messages"]
                                for block in item["content"]
                                if isinstance(block, dict)
                                and block.get("type") == "tool_result"
                                and block.get("tool_use_id") == owner.pending_tool
                            )
                        assert MARKER in json.dumps(result), result
                        owner.pending_tool = None
                    call_id = f"read-proof-{len(owner.requests)}"
                    if tool:
                        owner.pending_tool = call_id
                    if owner.mutate is not None:
                        changed = json.loads(owner.mutate.read_text(encoding="utf-8"))
                        changed["model"] = "changed-mid-run-must-not-be-used"
                        changed["limits"]["max_output_tokens"] = 1
                        write(owner.mutate, changed)
                        owner.mutate = None
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    for event in response(api, tool, call_id):
                        self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                    if api == "openai-completions":
                        self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except BaseException as error:
                    owner.errors.append(repr(error))

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.http.server_port}/v1"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


APIS = {
    "openai-completions": ("chat/completions", "max_completion_tokens"),
    "anthropic-messages": ("messages", "max_tokens"),
    "openai-responses": ("responses", "max_output_tokens"),
}


def response(api: str, tool: bool, call_id: str) -> list[dict[str, Any]]:
    arguments = json.dumps({"path": "proof.txt"})
    if api == "openai-responses":
        output = (
            [
                {
                    "type": "function_call",
                    "call_id": call_id,
                    "name": "read",
                    "arguments": arguments,
                }
            ]
            if tool
            else [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": MARKER}],
                }
            ]
        )
        return [
            {
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": output,
                    "usage": {"input_tokens": 20, "output_tokens": 10},
                },
            }
        ]
    if api == "openai-completions":
        delta = (
            {
                "tool_calls": [
                    {
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {"name": "read", "arguments": arguments},
                    }
                ]
            }
            if tool
            else {"content": MARKER}
        )
        return [
            {
                "choices": [
                    {"index": 0, "delta": delta, "finish_reason": "tool_calls" if tool else "stop"}
                ],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10},
            }
        ]
    block = (
        {"type": "tool_use", "id": call_id, "name": "read", "input": {}}
        if tool
        else {"type": "text", "text": ""}
    )
    delta = (
        {"type": "input_json_delta", "partial_json": arguments}
        if tool
        else {"type": "text_delta", "text": MARKER}
    )
    return [
        {"type": "message_start", "message": {"usage": {"input_tokens": 20}}},
        {"type": "content_block_start", "index": 0, "content_block": block},
        {"type": "content_block_delta", "index": 0, "delta": delta},
        {"type": "content_block_stop", "index": 0},
        {
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use" if tool else "end_turn"},
            "usage": {"output_tokens": 10},
        },
        {"type": "message_stop"},
    ]


class CustomWire:
    """External protocol with an observable provider socket cleanup barrier."""

    def __init__(self, mode: str) -> None:
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.closed = threading.Event()
        owner = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self) -> None:
                try:
                    self.connection.settimeout(30)
                    body = json.loads(self.rfile.readline())
                    assert body["version"] == "author-lines-v1"
                    owner.requests.append(body)
                    result = next(
                        (item for item in body["items"] if item["type"] == "tool_result"), None
                    )
                    if result:
                        assert result["call_id"] == "custom-read"
                        assert MARKER in result["result"]["text"]
                        item = {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "text", "text": MARKER}],
                        }
                    else:
                        item = {
                            "type": "tool_call",
                            "call_id": "custom-read",
                            "name": "read" if mode == "complete" else "write",
                            "arguments": json.dumps(
                                {"path": "proof.txt"}
                                if mode == "complete"
                                else {"path": "must-not-exist.txt", "content": "uncommitted tool"}
                            ),
                        }
                    frames = [
                        {"frame": "item", "item": item},
                        {"frame": "delta", "text": "CUSTOM_WIRE_GATE"},
                    ]
                    if mode == "complete":
                        frames.append({"frame": "commit"})
                    elif mode == "malformed":
                        frames.append({"frame": "unknown"})
                    for frame in frames:
                        self.wfile.write((json.dumps(frame) + "\n").encode())
                    self.wfile.flush()
                    if mode == "cancel":
                        assert self.rfile.read(1) == b"", "provider socket did not close"
                        owner.closed.set()
                except BaseException as error:
                    owner.errors.append(repr(error))

        class TCPServer(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self.server = TCPServer(("127.0.0.1", 0), Handler)
        self.address = f"127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        assert not self.errors, self.errors


def custom_cancel(
    command: list[str], caller: pathlib.Path, environment: dict[str, str], wire: CustomWire
) -> list[dict[str, Any]]:
    process = subprocess.Popen(
        command,
        cwd=caller,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    observed: list[str] = []
    gate = threading.Event()

    def collect() -> None:
        assert process.stdout is not None
        for line in process.stdout:
            observed.append(line)
            event = json.loads(line)
            if event["kind"] == "model_text_delta" and "CUSTOM_WIRE_GATE" in line:
                gate.set()

    reader = threading.Thread(target=collect, daemon=True)
    reader.start()
    try:
        assert gate.wait(30), "wire delta never reached installed host"
        process.send_signal(signal.SIGINT)
        assert wire.closed.wait(30), "cancel did not close provider socket"
        assert process.wait(timeout=30) == 130
        reader.join(timeout=5)
        assert not reader.is_alive()
        data = [json.loads(line) for line in observed]
        assert data[-1]["payload"]["outcome"]["status"] == "cancelled", data[-1]
        assert data[-1]["payload"]["cleanup_errors"] == []
        return data
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/model-access-install")
    host = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
    frozen = host.read_bytes()
    upstream_license = (
        (ROOT / "plugins/model-access/data/PI-LICENSE").read_text(encoding="utf-8").strip()
    )
    assert upstream_license in (destination / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    lib = library("author_model_services")
    folder = destination / "plugins/model-services/0.1.0"
    folder.mkdir(parents=True)
    shutil.copy2(author_artifact("model-services"), folder / lib)
    author = package(
        "model-services",
        [PROVIDER, CATALOG, MANAGER, CREDENTIALS],
        f"plugins/model-services/0.1.0/{lib}",
        target(),
    )
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-g1-installed-") as temporary:
        scratch = pathlib.Path(temporary)
        caller = scratch / "unrelated caller"
        caller.mkdir()
        project = scratch / "project"
        project.mkdir()
        (project / "proof.txt").write_text(MARKER, encoding="utf-8")
        global_dir = scratch / "global"
        global_dir.mkdir()
        write(global_dir / "settings.json", {"discover_skills": False, "discover_templates": False})
        environment = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(("OPENAI_", "ANTHROPIC_", "EDEN_API_"))
        }
        environment.update(EDEN_AUTHOR_SECRET=AUTHOR_SECRET, EDEN_AGENT_DIR=str(global_dir))
        server = Server()
        try:
            config = destination / "g1.json"
            selected = copy.deepcopy(composition)
            for item in selected["packages"]:
                if item["descriptor"]["package"] == "model-access":
                    item["config"] = {
                        "catalog": {
                            "source": server.base,
                            "models": [model(api, server.base) for api in APIS],
                        },
                        "credentials": {"path": str(global_dir / "keys.json")},
                    }
            write(config, selected)

            def invoke(arguments: list[str | pathlib.Path], stdin: str | None = None) -> str:
                command = [
                    str(arg)
                    for arg in [
                        host,
                        "--composition",
                        config,
                        "--cwd",
                        project,
                        "--global-dir",
                        global_dir,
                        *arguments,
                    ]
                ]
                completed = subprocess.run(
                    command,
                    cwd=caller,
                    env=environment,
                    input=stdin,
                    text=True,
                    capture_output=True,
                    timeout=60,
                )
                assert SECRET not in completed.stdout + completed.stderr
                assert AUTHOR_SECRET not in completed.stdout + completed.stderr
                assert completed.returncode == 0, (
                    command,
                    completed.stdout,
                    completed.stderr,
                    server.errors,
                )
                return completed.stdout

            def coding(history: pathlib.Path, prompt: str) -> None:
                data = [
                    json.loads(line)
                    for line in invoke(["--session", history, "--json", prompt]).splitlines()
                ]
                assert data[-1]["kind"] == "settled", data
                assert data[-1]["payload"]["outcome"]["status"] == "completed", data[-1]
                contents = history.read_text(encoding="utf-8")
                assert SECRET not in contents and AUTHOR_SECRET not in contents

            auth = json.loads(invoke(["auth", "set", "fixture"], SECRET + "\n"))
            assert auth["status"] == "completed", auth
            listing = json.loads(invoke(["models", "list"]))
            assert all(
                any(entry["target"]["model"] == api for entry in listing["models"]) for api in APIS
            )
            invoke(["models", "refresh"])
            assert server.catalog_gets > 0
            history = scratch / "cross-wire.jsonl"
            for api in APIS:
                invoke(["--session", history, "models", "select", "fixture", api])
                other_api = next(candidate for candidate in APIS if candidate != api)
                invoke(["models", "default", "fixture", other_api])
                current = json.loads(invoke(["--session", history, "models", "current"]))
                assert current["model"] == api, current
                server.tool_next = True
                before = len(server.requests)
                coding(history, "Read proof.txt and retain its exact content")
                assert len(server.requests) == before + 2
                coding(history, "Reopen and recall the prior read")
                invoke(["session", "compact", history])
                results[api] = {"coding_tool_roundtrip": True, "reopened": True, "compacted": True}

            target_path = scratch / "author-target.json"
            author["config"] = {"target_path": str(target_path)}
            for role in [CATALOG, CREDENTIALS]:
                replaced = copy.deepcopy(selected)
                replaced["packages"].append(author)
                replaced["roles"][role] = "model-services"
                write(config, replaced)
                write(target_path, model("openai-responses", server.base))
                replacement_history = scratch / (
                    "catalog.jsonl" if role == CATALOG else "credential.jsonl"
                )
                invoke(
                    [
                        "--session",
                        replacement_history,
                        "models",
                        "select",
                        "fixture",
                        "openai-responses",
                    ]
                )
                server.secret = SECRET if role == CATALOG else AUTHOR_SECRET
                server.tool_next = True
                if role == CATALOG:
                    server.mutate = target_path
                before = len(server.requests)
                coding(replacement_history, "Read proof.txt")
                assert len(server.requests) == before + 2
                results[role] = {
                    "default_provider_http_requests": 2,
                    "wire_limit": 777,
                    "frozen_route": role == CATALOG,
                }
            custom_results: dict[str, Any] = {}
            for mode in ["complete", "partial", "malformed", "cancel"]:
                if mode == "cancel" and os.name == "nt":
                    custom_results[mode] = (
                        "unaccepted: CLI Ctrl-C injection requires Windows console"
                    )
                    continue
                wire = CustomWire(mode)
                try:
                    custom = copy.deepcopy(selected)
                    custom_author = copy.deepcopy(author)
                    custom_author["config"] = {
                        "target_path": str(target_path),
                        "wire_address": wire.address,
                    }
                    custom["packages"].append(custom_author)
                    custom["roles"][PROVIDER] = "model-services"
                    write(config, custom)
                    custom_history = scratch / f"custom-{mode}.jsonl"
                    command = [
                        str(arg)
                        for arg in [
                            host,
                            "--composition",
                            config,
                            "--cwd",
                            project,
                            "--global-dir",
                            global_dir,
                            "--session",
                            custom_history,
                            "--json",
                            "Exercise independent wire",
                        ]
                    ]
                    if mode == "cancel":
                        data = custom_cancel(command, caller, environment, wire)
                    else:
                        completed = subprocess.run(
                            command,
                            cwd=caller,
                            env=environment,
                            text=True,
                            capture_output=True,
                            timeout=60,
                        )
                        data = [json.loads(line) for line in completed.stdout.splitlines()]
                        assert (completed.returncode == 0) == (mode == "complete"), (
                            completed.stdout,
                            completed.stderr,
                        )
                        assert data[-1]["payload"]["outcome"]["status"] == (
                            "completed" if mode == "complete" else "failed"
                        ), data[-1]
                    assert any(event["kind"] == "model_text_delta" for event in data)
                    assert not (project / "must-not-exist.txt").exists()
                    if mode != "complete":
                        assert '"kind":"tool_intent"' not in custom_history.read_text(
                            encoding="utf-8"
                        ).replace(" ", "")
                    assert len(wire.requests) == (2 if mode == "complete" else 1)
                    custom_results[mode] = {
                        "requests": len(wire.requests),
                        "uncommitted_tool_absent": True,
                        "socket_closed_observed": wire.closed.is_set(),
                    }
                finally:
                    wire.close()
            results["independent_new_wire"] = custom_results
            assert host.read_bytes() == frozen
        finally:
            server.close()
    evidence = {
        "host_unchanged": True,
        "independent_sdk_only_author": True,
        "secret_canaries_absent": True,
        "scenarios": results,
        "limitation": "controlled HTTP and independent TCP wire only; no live account acceptance",
    }
    write(ROOT / "artifacts/model-access-verification.json", evidence)
    print(json.dumps(evidence))


if __name__ == "__main__":
    main()
