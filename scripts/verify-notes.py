#!/usr/bin/env python3
"""Exercise installed notes, recall and cache warming against controlled HTTP."""

import copy
import http.server
import json
import os
import pathlib
import shutil
import threading
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT, library, package, target
from verification import author_artifact, example, installed, prepare, run


class Server:
    """Separate foreground, notes and warming requests and observe cancellation EOF."""

    def __init__(self, scratch: pathlib.Path, mode: str) -> None:
        self.errors: list[str] = []
        self.requests: list[dict[str, Any]] = []
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                assert self.path == "/release"
                owner.release.set()
                self.send_response(200)
                self.end_headers()

            def do_POST(self) -> None:
                try:
                    self.connection.settimeout(35)
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    owner.requests.append(body)
                    assert self.headers["Authorization"] == "Bearer controlled-notes-key"
                    budget = body.get("max_completion_tokens")
                    notes = budget == 2048
                    warm = budget == 1
                    if notes:
                        assert not body.get("tools"), "notes advertised tools"
                    if notes and mode == "failure":
                        self.send_response(400)
                        self.end_headers()
                        self.wfile.write(b'{"error":{"message":"injected notes failure"}}')
                        return
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.flush()
                    if warm or (notes and mode == "cancel"):
                        name = "warm" if warm else "notes"
                        (scratch / f"{name}-held").touch()
                        assert self.rfile.read(1) == b"", "auxiliary socket did not reach EOF"
                        (scratch / f"{name}-eof").touch()
                        return
                    if notes and mode in ("head-conflict", "combined"):
                        (scratch / "notes-held").touch()
                        assert owner.release.wait(30), "head mutation never released notes"
                    delta: dict[str, Any] = {
                        "content": "Durable notes: original rare details are available with history_recall."
                        if notes
                        else "foreground answer"
                    }
                    if notes and mode == "empty":
                        delta = {"content": "   "}
                    if notes and mode == "tool":
                        delta = {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "forbidden",
                                    "type": "function",
                                    "function": {
                                        "name": "write",
                                        "arguments": json.dumps(
                                            {"path": "must-not-exist.txt", "content": "forbidden"}
                                        ),
                                    },
                                }
                            ]
                        }
                    recalled = False
                    wants_recall = any(
                        message.get("role") == "user"
                        and "use restored notes and recall the old detail"
                        in json.dumps(message.get("content"), ensure_ascii=False)
                        for message in body.get("messages", [])
                    )
                    if not notes and not warm and wants_recall:
                        recalled = any(
                            message.get("role") == "tool"
                            and "rare-紫色-731"
                            in json.dumps(message.get("content"), ensure_ascii=False)
                            for message in body["messages"]
                        )
                        if recalled:
                            delta = {
                                "content": "Recovered original detail rare-紫色-731 through history_recall."
                            }
                        else:
                            assert any(
                                tool["function"]["name"] == "history_recall"
                                for tool in body.get("tools", [])
                            )
                            delta = {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "recall-original",
                                        "type": "function",
                                        "function": {
                                            "name": "history_recall",
                                            "arguments": json.dumps(
                                                {
                                                    "query": {
                                                        "operation": "search",
                                                        "literal": "rare-紫色-731",
                                                    }
                                                }
                                            ),
                                        },
                                    }
                                ]
                            }
                    frame = {
                        "choices": [
                            {
                                "index": 0,
                                "delta": delta,
                                "finish_reason": "tool_calls" if "tool_calls" in delta else "stop",
                            }
                        ],
                        "usage": {"prompt_tokens": 60001 if notes else 11, "completion_tokens": 3},
                    }
                    self.wfile.write(
                        ("data: " + json.dumps(frame) + "\n\ndata: [DONE]\n\n").encode()
                    )
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass
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


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/notes-install")
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    notes_library = destination / "plugins" / library("author_notes")
    shutil.copy2(author_artifact("notes"), notes_library)
    recall_author_library = destination / "plugins" / library("author_recall")
    shutil.copy2(author_artifact("recall"), recall_author_library)
    store_library = destination / "plugins" / library("author_coding_replacements")
    shutil.copy2(author_artifact("coding-replacements"), store_library)
    warmer_library = destination / "plugins" / library("author_cache_warmer")
    shutil.copy2(author_artifact("cache-warmer"), warmer_library)
    results: dict[str, Any] = {}
    for mode in (
        "manual",
        "threshold",
        "overflow",
        "post_response",
        "empty",
        "tool",
        "failure",
        "cancel",
        "head-conflict",
        "author-store",
        "store-failure",
        "combined",
    ):
        scratch = destination / mode
        scratch.mkdir()
        server = Server(scratch, mode)
        try:
            selected = copy.deepcopy(composition)
            bindings = {
                "eden.compaction-policy.v1": {"tail": "notes"},
                "eden.record-interpreter.v1": {"tail": "notes"},
                "eden.state-migrator.v1": {"tail": "notes"},
            }
            for item in selected["packages"]:
                name = item["descriptor"]["package"]
                item["library"] = str(destination / item["library"])
                if name == "notes":
                    item["library"] = str(notes_library)
                elif name == "cache-warmer":
                    item["library"] = str(warmer_library)
                    item["config"] = {
                        "mode": "idle" if mode in ("combined", "head-conflict") else "off",
                        "interval_ms": 100,
                        "ttl_ms": 120000,
                        "safety_ms": 40000,
                        "timeout_ms": 30000,
                        "max_requests": 1,
                        "max_output_tokens": 1,
                    }
                elif name == "coding-tools":
                    item["config"]["tools"].append("history_recall")
                    item["config"]["contributions"].append(
                        {
                            "catalog": "eden.history-recall-tools.v1",
                            "execute": "eden.history-recall-tool.v1",
                            "read_only": True,
                        }
                    )
                elif name == "coding":
                    item["config"] = {
                        "compaction": {"reserve_tokens": 100, "keep_recent_tokens": 100},
                        "retry": {"max_retries": 0},
                    }
                elif name == "model-access":
                    model = {
                        "provider": "openai",
                        "model": "notes-fixture",
                        "api": "openai-completions",
                        "base_url": server.base,
                        "limits": {
                            "context_window": 32768 if mode == "combined" else 65536,
                            "max_output_tokens": 4096,
                        },
                        "capabilities": {"tools": True, "images": False, "reasoning": False},
                        "source": {"kind": "author", "location": "controlled notes fixture"},
                    }
                    item["config"] = {
                        "catalog": {"models": [model]},
                        "credentials": {"path": str(scratch / "credentials.json")},
                    }
                    (scratch / "target.json").write_text(json.dumps(model), encoding="utf-8")
            selected["packages"].append(
                package(
                    "notes",
                    [
                        "eden.compaction-policy.v1",
                        "eden.record-interpreter.v1",
                        "eden.state-migrator.v1",
                        "eden.configuration.v1",
                    ],
                    str(notes_library),
                    target(),
                    {},
                )
            )
            selected["packages"].append(
                package(
                    "recall",
                    [
                        "eden.history-recall.v1",
                        "eden.history-recall-tools.v1",
                        "eden.history-recall-tool.v1",
                    ],
                    str(destination / "plugins/recall/0.1.0" / library("eden_recall")),
                    target(),
                    {},
                )
            )
            selected["packages"].append(
                package(
                    "recall-author",
                    ["test.recall-author.v1"],
                    str(recall_author_library),
                    target(),
                    {},
                )
            )
            if mode in ("author-store", "store-failure"):
                selected["packages"].append(
                    package(
                        "coding-replacements",
                        [
                            "eden.coding-provider.v1",
                            "eden.coding-context.v2",
                            "eden.coding-tool.v1",
                            "eden.session-store.v2",
                            "eden.record-interpreter.v1",
                            "eden.state-migrator.v1",
                        ],
                        str(store_library),
                        target(),
                        {"fail_kind": "compaction"} if mode == "store-failure" else {},
                    )
                )
                bindings["eden.session-store.v2"] = {"tail": "coding-replacements"}
            selected["roles"].update(
                {
                    role: "recall"
                    for role in (
                        "eden.history-recall.v1",
                        "eden.history-recall-tools.v1",
                        "eden.history-recall-tool.v1",
                    )
                }
            )
            selected["roles"]["test.recall-author.v1"] = "recall-author"
            selected["runtime"] = {"scopes": {"": {"bindings": bindings}}}
            default = copy.deepcopy(selected)
            default["packages"] = [
                item for item in default["packages"] if item["descriptor"]["package"] != "notes"
            ]
            default["runtime"]["scopes"][""]["bindings"] = {}
            (scratch / "default.json").write_text(json.dumps(default), encoding="utf-8")
            config = destination / f"{mode}.json"
            config.write_text(json.dumps(selected), encoding="utf-8")
            environment = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith(("OPENAI_", "ANTHROPIC_", "EDEN_API_"))
            }
            environment["OPENAI_API_KEY"] = "controlled-notes-key"
            completed = run(
                [example("notes_probe"), config, scratch, mode], ROOT, env=environment, timeout=120
            )
            results[mode] = json.loads(completed.stdout)
            if mode == "manual":
                resume_start = len(server.requests)
                reopened = run(
                    [example("notes_probe"), config, scratch, "resume"],
                    ROOT,
                    env=environment,
                    timeout=120,
                )
                results["resume"] = json.loads(reopened.stdout)
                for rejected_mode in ("missing-interpreter", "incompatible"):
                    rejected = scratch / rejected_mode
                    rejected.mkdir()
                    shutil.copy2(
                        scratch
                        / (
                            "future-v2.jsonl"
                            if rejected_mode == "incompatible"
                            else "history.jsonl"
                        ),
                        rejected / "history.jsonl",
                    )
                    count = len(server.requests)
                    refused = run(
                        [
                            example("notes_probe"),
                            config if rejected_mode == "incompatible" else scratch / "default.json",
                            rejected,
                            rejected_mode,
                        ],
                        ROOT,
                        env=environment,
                        timeout=120,
                    )
                    results[rejected_mode] = json.loads(refused.stdout)
                    assert len(server.requests) == count, "unsafe recovery reached the provider"

                resumed = server.requests[resume_start]
                assert "Durable notes:" in json.dumps(resumed)
                assert "rare-紫色-731" not in json.dumps(resumed, ensure_ascii=False), (
                    "original history was fully restored into prompt"
                )
            if mode == "cancel":
                assert (scratch / "notes-eof").exists()
            notes_requests = sum(
                request.get("max_completion_tokens") == 2048 for request in server.requests
            )
            assert notes_requests == 1, (mode, notes_requests)
            if mode == "combined":
                assert (
                    sum(request.get("max_completion_tokens") == 1 for request in server.requests)
                    >= 1
                )
            results[mode]["notes_requests"] = notes_requests
        finally:
            server.close()
    evidence = {
        "independent_actual_notes_plugin": True,
        "scenarios": results,
        "limitation": "Controlled HTTP proves contracts; no live-model note quality or cache-hit claim.",
    }
    (ROOT / "artifacts/notes-verification.json").write_text(
        json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(evidence))


if __name__ == "__main__":
    main()
