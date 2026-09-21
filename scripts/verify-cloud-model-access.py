#!/usr/bin/env python3
"""Installed G2 proof of native cloud wires, durable replay, and private keys."""

import base64
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT
from verification import installed, prepare

APIS = (
    "google-generative-ai",
    "google-vertex",
    "azure-openai-responses",
    "mistral-conversations",
)
SECRET = "G2-PRIVATE-stored-key-canary"
MARKERS = ("first-cloud-read-content", "second-distinct-cloud-read-content")
PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII="
DEPLOYMENT = "explicit-azure-deployment"


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def google(api: str) -> bool:
    return api.startswith("google-")


def response(api: str, turn: int) -> dict[str, Any]:
    """Distinct reads expose accidental reuse of synthesized tool-call identities."""
    tool = turn < 3
    path = ("first.txt", "second.txt", "pixel.png")[turn] if tool else ""
    arguments = {"path": path}
    if google(api):
        part = (
            {
                "functionCall": {"name": "read", "args": arguments},
                "thoughtSignature": f"fixture-signature-{turn}",
            }
            if tool
            else {"text": "cloud proof complete"}
        )
        return {
            "candidates": [{"content": {"parts": [part]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 10},
        }
    if api == "azure-openai-responses":
        output = (
            {
                "type": "function_call",
                "call_id": f"cloud-read-{turn}",
                "name": "read",
                "arguments": json.dumps(arguments),
            }
            if tool
            else {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "cloud proof complete"}],
            }
        )
        return {
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [output],
                "usage": {"input_tokens": 20, "output_tokens": 10},
            },
        }
    delta = (
        {
            "tool_calls": [
                {
                    "index": 0,
                    "id": f"cloud000{turn}",
                    "type": "function",
                    "function": {"name": "read", "arguments": arguments},
                }
            ]
        }
        if tool
        else {"content": [{"type": "text", "text": "cloud proof complete"}]}
    )
    return {
        "choices": [
            {"index": 0, "delta": delta, "finish_reason": "tool_calls" if tool else "stop"}
        ],
        "usage": {"prompt_tokens": 20, "completion_tokens": 10},
    }


def verify_replay(api: str, body: dict[str, Any], turn: int) -> None:
    if google(api):
        parts = [part for item in body["contents"] for part in item["parts"]]
        results = [part["functionResponse"] for part in parts if "functionResponse" in part]
        for index in range(min(turn, 3)):
            signed = [
                part
                for part in parts
                if part.get("thoughtSignature") == f"fixture-signature-{index}"
            ]
            assert len(signed) == 1, signed
            assert signed[0]["functionCall"] == {
                "name": "read",
                "args": {"path": ("first.txt", "second.txt", "pixel.png")[index]},
            }, signed
        if turn >= 3:
            assert len(results) == 3, results
            assert results[2]["name"] == "read", results[2]
            assert results[2]["parts"] == [
                {"inlineData": {"mimeType": "image/png", "data": PNG}}
            ], results[2]
            assert all("parts" not in result for result in results[:2]), results
            assert all("inlineData" not in part for part in parts), parts
    elif api == "azure-openai-responses":
        results = [item for item in body["input"] if item.get("type") == "function_call_output"]
        assert [item["call_id"] for item in results] == [
            f"cloud-read-{i}" for i in range(min(turn, 3))
        ]
        if turn >= 3:
            assert isinstance(results[2]["output"], list), results[2]
            images = [
                part
                for part in results[2]["output"]
                if isinstance(part, dict) and part.get("type") == "input_image"
            ]
            assert images == [
                {"type": "input_image", "image_url": f"data:image/png;base64,{PNG}"}
            ], images
    else:
        results = [item for item in body["messages"] if item.get("role") == "tool"]
        assert [item["tool_call_id"] for item in results] == [
            f"cloud000{i}" for i in range(min(turn, 3))
        ]
        if turn >= 3:
            images = [
                part
                for item in results
                for part in item["content"]
                if part.get("type") == "image_url"
            ]
            assert any(part["image_url"] == f"data:image/png;base64,{PNG}" for part in images), (
                images
            )
    assert len(results) == min(turn, 3), results
    for index in range(min(turn, 2)):
        assert MARKERS[index] in json.dumps(results[index]), results[index]


class Server:
    def __init__(self) -> None:
        self.requests: dict[str, list[dict[str, Any]]] = {api: [] for api in APIS}
        self.errors: list[str] = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    # Gemini omits model in its payload: dispatch by the native resource path.
                    api = next(
                        (
                            api
                            for api in APIS
                            if google(api) and f"/{api}:streamGenerateContent" in self.path
                        ),
                        None,
                    )
                    if api is None:
                        api = (
                            "azure-openai-responses"
                            if body.get("model") == DEPLOYMENT
                            else body["model"]
                        )
                    assert api in APIS, api
                    turn = len(owner.requests[api])
                    assert turn < 5, (api, turn)
                    assert self.headers["x-fixture-route"] == "cloud-installed"
                    if google(api):
                        prefix = "publishers/google/" if api == "google-vertex" else ""
                        assert (
                            self.path == f"/v1/{prefix}models/{api}:streamGenerateContent?alt=sse"
                        ), self.path
                        assert self.headers["x-goog-api-key"] == SECRET
                        assert body["generationConfig"]["maxOutputTokens"] == 777
                        assert "model" not in body
                        assert self.headers.get("Authorization") is None
                    elif api == "azure-openai-responses":
                        assert self.path == "/v1/responses?api-version=2025-04-01-preview", (
                            self.path
                        )
                        assert self.headers["api-key"] == SECRET
                        assert self.headers.get("Authorization") is None
                        assert body["max_output_tokens"] == 777
                    else:
                        assert self.path == "/v1/chat/completions", self.path
                        assert self.headers["Authorization"] == f"Bearer {SECRET}"
                        assert body["max_tokens"] == 777
                    verify_replay(api, body, turn)
                    owner.requests[api].append(body)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(("data: " + json.dumps(response(api, turn)) + "\n\n").encode())
                    if api == "mistral-conversations":
                        self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except BaseException as error:
                    owner.errors.append(repr(error).replace(SECRET, "[redacted]"))

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.http.server_port}/v1"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/cloud-model-access-install")
    host = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
    frozen = host.read_bytes()
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-g2-installed-") as temporary:
        scratch = pathlib.Path(temporary)
        caller, project, global_dir = (
            scratch / name for name in ("unrelated caller", "project", "global")
        )
        for folder in (caller, project, global_dir):
            folder.mkdir()
        for name, marker in zip(("first.txt", "second.txt"), MARKERS, strict=True):
            (project / name).write_text(marker, encoding="utf-8")
        (project / "pixel.png").write_bytes(base64.b64decode(PNG))
        write(global_dir / "settings.json", {"discover_skills": False, "discover_templates": False})
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("GOOGLE_", "GEMINI_", "AZURE_", "MISTRAL_", "EDEN_API_"))
        }
        environment["EDEN_AGENT_DIR"] = str(global_dir)
        server = Server()
        try:
            config = destination / "g2.json"
            models = [
                {
                    "provider": "cloud-fixture",
                    "model": api,
                    "api": api,
                    "base_url": server.base,
                    "headers": {"x-fixture-route": "cloud-installed"},
                    "limits": {"context_window": 65536, "max_output_tokens": 777},
                    "capabilities": {"tools": True, "images": True, "reasoning": False},
                    "source": {"kind": "custom", "location": "installed cloud fixture"},
                    "compat": {"deployment": DEPLOYMENT, "apiVersion": "2025-04-01-preview"}
                    if api == "azure-openai-responses"
                    else {},
                }
                for api in APIS
            ]
            for package in composition["packages"]:
                if package["descriptor"]["package"] == "model-access":
                    package["config"] = {
                        "catalog": {"offline": True, "models": models},
                        "credentials": {"path": str(global_dir / "keys.json")},
                    }
            write(config, composition)

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
                assert SECRET not in completed.stdout + completed.stderr, (
                    "secret leaked to CLI output"
                )
                assert completed.returncode == 0, (
                    command,
                    completed.stdout,
                    completed.stderr,
                    server.errors,
                )
                return completed.stdout

            auth = json.loads(invoke(["auth", "set", "cloud-fixture"], SECRET + "\n"))
            assert auth["status"] == "completed", auth
            for api in APIS:
                history = scratch / f"{api}.jsonl"
                invoke(["--session", history, "models", "select", "cloud-fixture", api])
                for expected, prompt in (
                    (4, "Read first.txt, second.txt, and pixel.png"),
                    (5, "Recall the prior reads and image"),
                ):
                    events = [
                        json.loads(line)
                        for line in invoke(["--session", history, "--json", prompt]).splitlines()
                    ]
                    assert events[-1]["kind"] == "settled", events
                    assert events[-1]["payload"]["outcome"]["status"] == "completed", events[-1]
                    assert len(server.requests[api]) == expected, (
                        api,
                        server.requests[api],
                        server.errors,
                    )
                    contents = history.read_text(encoding="utf-8")
                    assert SECRET not in contents, "secret leaked to history"
                    assert all(marker in contents for marker in MARKERS), contents
                results[api] = {
                    "coding_tool_roundtrip": True,
                    "distinct_successive_reads": True,
                    "native_image_projection": True,
                    "reopened": True,
                    "private_stored_key": True,
                    "requests": len(server.requests[api]),
                }
                if google(api):
                    results[api]["signed_idless_turns_replayed"] = True
                if api == "azure-openai-responses":
                    results[api]["explicit_deployment_and_api_version"] = True
        finally:
            server.close()
    assert host.read_bytes() == frozen, "installed host changed during verification"
    write(
        ROOT / "artifacts/cloud-model-access-verification.json",
        {"status": "passed", "protocols": results},
    )
    print("Installed cloud model access verification passed")


if __name__ == "__main__":
    main()
