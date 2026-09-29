#!/usr/bin/env python3
"""Installed shared context edits and frozen references against controlled HTTP."""

import argparse
import base64
import copy
import http.server
import importlib.util
import json
import os
import pathlib
import platform
import shutil
import struct
import tempfile
import zlib
from typing import Any

from install import ROOT, library, package, target
from verification import author_artifact, example, installed, prepare, run

# Keep provider framing, native configuration and gate behavior in the existing fixture.
_spec = importlib.util.spec_from_file_location(
    "context_fixture", ROOT / "scripts/verify-context.py"
)
assert _spec is not None and _spec.loader is not None
fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(fixture)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--scenario",
        action="append",
        choices=[
            "persistent",
            "next-request",
            "in-flight",
            "compact-rebuild",
            "notes",
            "reference-same",
            "reference-different",
            "reference-unknown",
            "reference-budget",
            "policy-branch",
            "policy-next_request",
            "policy-fail",
            "policy-invalid",
            "policy-run-finish",
            "large-off",
            "large-on",
            "model-images",
        ],
    )
    requested = parser.parse_args().scenario

    def selected(mode: str) -> bool:
        return requested is None or mode in requested

    os.environ["EDEN_CONTEXT_KEY"] = "context-verifier-key"
    prepared = prepare()
    destination = installed(ROOT / "artifacts/shared-context-install")
    probe = example("shared_context_probe")
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-shared-context-") as temp:
        scratch = pathlib.Path(temp)
        project = scratch / "project"
        project.mkdir()
        global_dir = scratch / "global"
        global_dir.mkdir()
        os.environ["EDEN_AGENT_DIR"] = str(global_dir)
        fixture.write(
            global_dir / "settings.json", {"discover_skills": False, "discover_templates": False}
        )

        def invoke(
            config: pathlib.Path,
            history: pathlib.Path,
            mode: str,
            server: Any,
            source: pathlib.Path | None = None,
        ) -> dict[str, Any]:
            args = [probe, config, project, history, mode, server.address]
            if source is not None:
                args.append(source)
            return json.loads(run(args, scratch, timeout=180).stdout)

        for mode in ["persistent", "next-request", "in-flight", "compact-rebuild", "notes"]:
            if not selected(mode) and not (
                mode == "persistent"
                and requested
                and any(value.startswith("reference-") for value in requested)
            ):
                continue
            history = scratch / f"{mode}.jsonl"
            count_file = project / "execution-count.txt"
            count_file.unlink(missing_ok=True)

            def respond(
                body: dict[str, Any], index: int, mode: str = mode
            ) -> tuple[int, dict[str, Any]]:
                if mode == "notes":
                    tool = index == 0
                    if tool:
                        delta = {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "effect",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": json.dumps(
                                            {"command": "printf x >> execution-count.txt"}
                                        ),
                                    },
                                }
                            ]
                        }
                    elif not body.get("tools"):
                        serialized = json.dumps(body["messages"])
                        assert (
                            "EDITED-CONTEXT" in serialized and "ORIGINAL-CONTEXT" not in serialized
                        )
                        delta = {"content": "EDITED-SUMMARY"}
                    else:
                        delta = {"content": "source response " + "retained information " * 400}
                    return 200, {
                        "choices": [
                            {
                                "index": 0,
                                "delta": delta,
                                "finish_reason": "tool_calls" if tool else "stop",
                            }
                        ],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
                    }
                if mode == "next-request" and index == 1:
                    return 503, {"error": {"code": "server_error"}}
                if (mode == "in-flight" and index == 1) or (
                    mode in ["compact-rebuild", "notes"] and index == 0
                ):
                    return 200, fixture.complete(fixture.effect())
                if fixture.summary(body):
                    serialized = json.dumps(body["input"])
                    assert "EDITED-CONTEXT" in serialized and "ORIGINAL-CONTEXT" not in serialized
                    return 200, fixture.complete(fixture.answer("EDITED-SUMMARY"))
                return 200, fixture.complete(
                    fixture.answer("source response " + "retained information " * 400)
                )

            server = fixture.Server(respond, gate=1 if mode == "in-flight" else None)
            try:
                selected_composition = copy.deepcopy(composition)
                if mode == "notes":
                    name = "note-style-context-management"
                    roles = [
                        "eden.compaction-policy.v1",
                        "eden.record-interpreter.v1",
                        "eden.state-migrator.v1",
                        "eden.configuration.v1",
                        "eden.history-recall.v1",
                        "eden.history-recall-tools.v1",
                        "eden.history-recall-tool.v1",
                    ]
                    native = (
                        destination
                        / "plugins"
                        / name
                        / "0.1.0"
                        / library("eden_note_style_context_management")
                    )
                    assert native.is_file(), native
                    selected_composition["packages"].append(
                        package(name, roles, str(native), target(), {})
                    )
                    selected_composition["runtime"] = {
                        "scopes": {"": {"bindings": {role: {"tail": name} for role in roles[:3]}}}
                    }
                config = fixture.configure(
                    destination, selected_composition, server, mode, keep_recent=1
                )
                if mode == "notes":
                    configured = json.loads(config.read_text(encoding="utf-8"))
                    catalog = scratch / "notes-catalog.json"
                    fixture.write(
                        catalog, {"default": {"provider": "openai", "model": "controlled-model"}}
                    )
                    for item in configured["packages"]:
                        if item["descriptor"]["package"] == "model-access":
                            item["config"] = {
                                "catalog": {
                                    "offline": True,
                                    "cache_path": str(catalog),
                                    "models": [
                                        {
                                            "provider": "openai",
                                            "model": "controlled-model",
                                            "api": "openai-completions",
                                            "base_url": f"http://{server.address}",
                                            "limits": {
                                                "context_window": 1048576,
                                                "max_output_tokens": 393216,
                                            },
                                            "capabilities": {
                                                "tools": True,
                                                "images": True,
                                                "reasoning": False,
                                            },
                                            "source": {
                                                "kind": "fixture",
                                                "location": "controlled notes target",
                                            },
                                        }
                                    ],
                                },
                                "credentials": {
                                    "providers": {"openai": {"env": "EDEN_CONTEXT_KEY"}}
                                },
                            }
                    fixture.write(config, configured)
                report = invoke(
                    config, history, "compact-rebuild" if mode == "notes" else mode, server
                )
                inputs = [
                    json.dumps(body.get("input", body.get("messages"))) for body in server.requests
                ]
                assert "ORIGINAL-CONTEXT" in inputs[0]
                if mode == "persistent":
                    assert "EDITED-CONTEXT" in inputs[1] and "ORIGINAL-CONTEXT" not in inputs[1]
                    report["reopen"] = invoke(config, history, "reopen", server)
                    reopened = json.dumps(server.requests[-1]["input"])
                    assert "EDITED-CONTEXT" in reopened and "ORIGINAL-CONTEXT" not in reopened
                elif mode == "next-request":
                    assert len(inputs) == 4
                    assert server.requests[1]["input"] == server.requests[2]["input"]
                    assert "EDITED-CONTEXT" in inputs[1] and "ORIGINAL-CONTEXT" not in inputs[1]
                    assert "ORIGINAL-CONTEXT" in inputs[3] and "EDITED-CONTEXT" not in inputs[3]
                elif mode == "in-flight":
                    assert len(inputs) == 3
                    assert "ORIGINAL-CONTEXT" in inputs[1] and "EDITED-CONTEXT" not in inputs[1]
                    assert "EDITED-CONTEXT" in inputs[2] and "ORIGINAL-CONTEXT" not in inputs[2]
                    assert count_file.read_text() == "x"
                else:
                    assert len(inputs) == 6
                    assert fixture.summary(server.requests[3])
                    assert "EDITED-SUMMARY" in inputs[4] and "ORIGINAL-CONTEXT" not in inputs[4]
                    assert "ORIGINAL-CONTEXT" in inputs[5]
                    assert "EDITED-SUMMARY" not in inputs[5], (
                        "rebuild retained old notes or compaction summary"
                    )
                    assert count_file.read_text() == "x", "rebuild replayed historical tools"
                    durable = fixture.records(history)
                    assert sum(record["kind"] == "tool_intent" for record in durable) == 1
                    if mode == "notes":
                        assert any(
                            record["kind"] == "extension_state"
                            and record["payload"].get("namespace") == "eden.notes"
                            for record in durable
                        )
                        report["notes_checkpoint"] = True
                report["provider_requests"] = len(server.requests)
                results[mode] = report
            finally:
                server.close()

        source = scratch / "persistent.jsonl"
        source_bytes = source.read_bytes() if source.exists() else b""
        for mode in [
            "reference-same",
            "reference-different",
            "reference-unknown",
            "reference-budget",
        ]:
            if not selected(mode):
                continue
            server = fixture.Server(
                lambda *_: (200, fixture.complete(fixture.answer("quoted source accepted")))
            )
            try:
                config = fixture.configure(
                    destination,
                    composition,
                    server,
                    mode,
                    window=4096 if mode == "reference-budget" else 1048576,
                    allowance=512 if mode == "reference-budget" else 393216,
                )
                report = invoke(config, scratch / f"{mode}.jsonl", mode, server, source)
                assert source.read_bytes() == source_bytes
                if mode == "reference-budget":
                    assert not server.requests, "over-budget reference contacted provider"
                else:
                    assert len(server.requests) == 1
                    body = json.dumps(server.requests[0]["input"])
                    assert "EDITED-CONTEXT" in body and "ORIGINAL-CONTEXT" not in body
                    assert ("Source final system prompt differs" in body) is (
                        mode == "reference-different"
                    )
                    assert ("Source final system prompt: unknown" in body) is (
                        mode == "reference-unknown"
                    )
                    assert all(
                        item.get("type") not in ["function_call", "function_call_output"]
                        for item in server.requests[0]["input"]
                    )
                report["provider_requests"] = len(server.requests)
                report["source_unchanged"] = True
                results[mode] = report
            finally:
                server.close()
        for mode in [
            "policy-branch",
            "policy-next_request",
            "policy-fail",
            "policy-invalid",
            "policy-run-finish",
        ]:
            if not selected(mode):
                continue
            server = fixture.Server(
                lambda *_: (200, fixture.complete(fixture.answer("policy response")))
            )
            try:
                config = fixture.configure(destination, composition, server, mode)
                configured = json.loads(config.read_text(encoding="utf-8"))
                native = destination / "plugins" / library("author_context_edits")
                shutil.copy2(author_artifact("context-edits"), native)
                role = "test.context-policy.v1"
                configured["packages"].append(
                    package("context-edits", [role], str(native), target(), {})
                )
                configured["roles"][role] = "context-edits"
                scope = "next_request" if mode == "policy-next_request" else "branch"
                operations = (
                    ["first", "second"]
                    if mode in ["policy-branch", "policy-next_request"]
                    else ["first", mode.removeprefix("policy-")]
                )
                boundary = "run_finish" if mode == "policy-run-finish" else "before_request"
                if mode == "policy-run-finish":
                    operations = ["first"]
                for item in configured["packages"]:
                    if item["descriptor"]["package"] == "coding":
                        item["config"]["context_policies"] = [
                            {
                                "name": operation,
                                "role": role,
                                "boundary": boundary,
                                "enabled": True,
                                "config": {
                                    "operation": operation,
                                    "scope": scope,
                                    "expected_boundary": boundary,
                                },
                            }
                            for operation in operations
                        ]
                fixture.write(config, configured)
                history = scratch / f"{mode}.jsonl"
                report = invoke(config, history, mode, server)
                if mode in ["policy-fail", "policy-invalid"]:
                    assert not server.requests, "failed policy sent fallback input"
                    assert not any(
                        record["kind"] == "model_request" for record in fixture.records(history)
                    )
                elif mode == "policy-run-finish":
                    assert len(server.requests) == 2
                    assert all(
                        "POLICY-FIRST" not in json.dumps(body["input"]) for body in server.requests
                    )
                    assert "FOLLOW-UP-BEFORE-FINISH" in json.dumps(server.requests[1]["input"])
                    for item in configured["packages"]:
                        if item["descriptor"]["package"] == "coding":
                            item["config"]["context_policies"] = []
                    fixture.write(config, configured)
                    invoke(config, history, "policy-reopen-run_finish", server)
                    assert len(server.requests) == 3
                    assert "POLICY-FIRST" in json.dumps(server.requests[2]["input"])
                else:
                    assert len(server.requests) == 1
                    assert server.requests[0]["tools"] == []
                    assert "POLICY-FIRST-SECOND" in json.dumps(server.requests[0]["input"])
                    # Reopen with policy execution disabled to distinguish the persisted scope.
                    for item in configured["packages"]:
                        if item["descriptor"]["package"] == "coding":
                            item["config"]["context_policies"] = []
                    fixture.write(config, configured)
                    reopened = invoke(config, history, f"policy-reopen-{scope}", server)
                    assert ("POLICY-FIRST-SECOND" in json.dumps(server.requests[1]["input"])) is (
                        scope == "branch"
                    )
                    report["reopen"] = {"records": reopened["records"], "scope": scope}
                report.pop("effective", None)
                report["provider_requests"] = len(server.requests)
                results[mode] = report
            finally:
                server.close()

        for mode in ["large-off", "large-on"]:
            if not selected(mode):
                continue

            def large_response(_: dict[str, Any], index: int) -> tuple[int, dict[str, Any]]:
                if index == 0:
                    return 200, fixture.complete(
                        [
                            {
                                "type": "function_call",
                                "call_id": "large-output",
                                "name": "bash",
                                "arguments": json.dumps(
                                    {
                                        "command": "printf 'RAW-START'; printf '%020000d' 0; printf 'RAW-END'"
                                    }
                                ),
                            }
                        ]
                    )
                return 200, fixture.complete(fixture.answer("large result accepted"))

            server = fixture.Server(large_response)
            try:
                config = fixture.configure(destination, composition, server, mode)
                if mode == "large-on":
                    configured = json.loads(config.read_text(encoding="utf-8"))
                    for item in configured["packages"]:
                        if item["descriptor"]["package"] == "coding":
                            item["config"]["context_policies"] = [
                                {
                                    "name": "large-tool-output",
                                    "role": "large_tool_output",
                                    "boundary": "before_request",
                                    "enabled": True,
                                    "config": {
                                        "threshold_chars": 1000,
                                        "keep_chars": 200,
                                        "keep_recent_results": 0,
                                    },
                                }
                            ]
                    fixture.write(config, configured)
                history = scratch / f"{mode}.jsonl"
                invoke(config, history, mode, server)
                assert len(server.requests) == 2
                output = next(
                    item["output"]
                    for item in server.requests[1]["input"]
                    if item.get("type") == "function_call_output"
                )
                raw = next(
                    record["payload"]["result"]["text"]
                    for record in fixture.records(history)
                    if record["kind"] == "tool_result"
                )
                assert "RAW-START" in raw and "RAW-END" in raw and len(raw) > 20000
                assert ("Earlier output shortened" in output) is (mode == "large-on")
                assert len(output) < 1000 if mode == "large-on" else len(output) > 20000
                results[mode] = {
                    "provider_requests": 2,
                    "raw_characters": len(raw),
                    "sent_characters": len(output),
                    "original_preserved": True,
                }
            finally:
                server.close()
        if selected("model-images"):
            # Keep the existing server lifecycle; this route permits three catalog model names.
            server = fixture.Server(
                lambda *_: (200, fixture.complete(fixture.answer("image accepted")))
            )

            class ModelHandler(http.server.BaseHTTPRequestHandler):
                def log_message(self, format: str, *args: object) -> None:
                    pass

                def do_GET(self) -> None:
                    server.markers.append({"path": self.path, "requests": len(server.requests)})
                    self.send_response(200)
                    self.end_headers()

                def do_POST(self) -> None:
                    try:
                        assert self.headers["Authorization"] == "Bearer context-verifier-key"
                        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                        assert (
                            body["model"] in ["vision", "tiny", "text"] and body["stream"] is True
                        )
                        server.requests.append(body)
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        payload = fixture.complete(fixture.answer("image accepted"))
                        self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
                        self.wfile.flush()
                    except BaseException as error:
                        server.errors.append(repr(error))

            server.http.RequestHandlerClass = ModelHandler
            try:
                configured = copy.deepcopy(composition)
                credentials = scratch / "model-image-credentials.json"
                for item in configured["packages"]:
                    if item["descriptor"]["package"] == "model-access":
                        item["config"] = {
                            "catalog": {
                                "models": [
                                    {
                                        "provider": "fixture",
                                        "model": name,
                                        "api": "openai-responses",
                                        "base_url": f"http://{server.address}",
                                        "limits": {
                                            "context_window": 1048576,
                                            "max_output_tokens": 4096,
                                        },
                                        "capabilities": {
                                            "tools": True,
                                            "images": name != "text",
                                            "reasoning": False,
                                        },
                                        "source": {
                                            "kind": "author",
                                            "location": "controlled shared-context image fixture",
                                        },
                                        "compat": {
                                            "image_limits": {
                                                "max_width": 1 if name == "tiny" else 3,
                                                "max_height": 1 if name == "tiny" else 3,
                                            }
                                        },
                                    }
                                    for name in ["vision", "tiny", "text"]
                                ]
                            },
                            "credentials": {"path": str(credentials)},
                        }
                    elif item["descriptor"]["package"] == "coding":
                        item["config"] = {
                            "compaction": {
                                "reserve_tokens": 8192,
                                "keep_recent_tokens": 20000,
                                "models": {"fixture": {"vision": {"reserve_tokens": 1024}}},
                            },
                            "images": {
                                "models": {
                                    "fixture": {
                                        "vision": {"limits": {"max_width": 2, "max_height": 2}},
                                        "tiny": {"limits": {"max_width": 10, "max_height": 10}},
                                    }
                                }
                            },
                        }
                config = destination / "model-images.json"
                fixture.write(config, configured)

                def chunk(name: bytes, value: bytes) -> bytes:
                    return (
                        struct.pack(">I", len(value))
                        + name
                        + value
                        + struct.pack(">I", zlib.crc32(name + value))
                    )

                pixels = b"".join(b"\0" + bytes([row * 60, 100, 200]) * 4 for row in range(4))
                png = (
                    b"\x89PNG\r\n\x1a\n"
                    + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0))
                    + chunk(b"IDAT", zlib.compress(pixels))
                    + chunk(b"IEND", b"")
                )
                blocks_path = scratch / "image-blocks.json"
                fixture.write(
                    blocks_path,
                    [
                        {"type": "text", "text": "Remember this original image"},
                        {
                            "type": "image",
                            "media_type": "image/png",
                            "data": base64.b64encode(png).decode(),
                        },
                    ],
                )
                report = invoke(
                    config, scratch / "model-images.jsonl", "model-images", server, blocks_path
                )
                assert [body["model"] for body in server.requests] == ["vision", "tiny", "text"]
                assert server.markers == [
                    {"path": "/tiny-rejected", "requests": 1},
                    {"path": "/text-rejected", "requests": 2},
                ]
                images = [
                    [
                        block["image_url"]
                        for item in body["input"]
                        for block in item.get("content", [])
                        if block.get("type") == "input_image"
                    ]
                    for body in server.requests
                ]
                assert len(images[0]) == 1 and len(images[1]) == 1 and images[2] == []
                assert images[0] != images[1]
                retained = next(
                    record["payload"]["image"]
                    for record in reversed(fixture.records(scratch / "model-images.jsonl"))
                    if record["kind"] == "image_version"
                )
                for index in range(2):
                    payload = retained["payloads"][retained["versions"][index]["payload"]]
                    assert (
                        images[index][0] == f"data:{payload['media_type']};base64,{payload['data']}"
                    )
                for urls, expected in [(images[0], 2), (images[1], 1)]:
                    sent = base64.b64decode(urls[0].split(",", 1)[1])
                    assert sent[:8] == b"\x89PNG\r\n\x1a\n"
                    assert struct.unpack(">II", sent[16:24]) == (expected, expected)
                report["provider_requests"] = len(server.requests)
                report["rejections_before_provider"] = server.markers
                results["model-images"] = report
            finally:
                server.close()

    evidence = {
        "platform": platform.platform(),
        "target": target(),
        "commit": prepared["commit"],
        "source_dirty": prepared["dirty"],
        "provider_evidence": "controlled HTTP using installed default native plugins and public Session APIs",
        "scenarios": results,
    }
    fixture.write(ROOT / "artifacts/shared-context-verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
