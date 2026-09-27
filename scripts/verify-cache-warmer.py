#!/usr/bin/env python3
"""Installed cache warming mechanics; controlled usage is not real cache-hit evidence."""

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

CANARY = "AUXILIARY-ONLY-CANARY"


class Server:
    """Hold an auxiliary socket until cancellation and observe actual peer EOF."""

    def __init__(self, scratch: pathlib.Path, mode: str) -> None:
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.held = threading.Event()
        self.closed = threading.Event()
        self.auxiliary = 0
        self.foreground = 0
        self.prefix: dict[str, Any] | None = None
        self.lock = threading.Lock()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                try:
                    self.connection.settimeout(35)
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    assert self.path == "/v1/chat/completions", self.path
                    assert self.headers["Authorization"] == "Bearer controlled-cache-key"
                    assert "AUTHOR-WRAPPER-PREFIX" in json.dumps(body), body
                    auxiliary = body["max_completion_tokens"] == 1
                    with owner.lock:
                        owner.requests.append(body)
                        if auxiliary:
                            owner.auxiliary += 1
                            number = owner.auxiliary
                            assert owner.prefix is not None
                            expected = copy.deepcopy(owner.prefix)
                            expected["max_completion_tokens"] = 1
                            assert body == expected, (body, expected)
                        else:
                            owner.foreground += 1
                            number = owner.foreground
                            owner.prefix = body
                    held = auxiliary and number == (2 if mode == "idle" else 1)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.flush()
                    if held:
                        (scratch / "aux-held").touch()
                        owner.held.set()
                        assert self.rfile.read(1) == b"", "auxiliary socket did not reach EOF"
                        owner.closed.set()
                        (scratch / "aux-eof").touch()
                        return
                    if not auxiliary and number == 1 and mode == "streaming":
                        assert owner.held.wait(30), "no warming during foreground streaming"
                    if not auxiliary and number == 2 and mode == "idle":
                        assert owner.closed.wait(30), (
                            "new foreground did not retire auxiliary socket"
                        )
                    delta = (
                        {
                            "content": CANARY,
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "never-execute",
                                    "type": "function",
                                    "function": {
                                        "name": "write",
                                        "arguments": json.dumps(
                                            {"path": "must-not-exist.txt", "content": CANARY}
                                        ),
                                    },
                                }
                            ],
                        }
                        if auxiliary
                        else {"content": "foreground answer"}
                    )
                    frame = {
                        "choices": [
                            {
                                "index": 0,
                                "delta": delta,
                                "finish_reason": "tool_calls" if auxiliary else "stop",
                            }
                        ],
                        "usage": {
                            "prompt_tokens": 901 if auxiliary else 11,
                            "completion_tokens": 1 if auxiliary else 3,
                            "prompt_tokens_details": {"cached_tokens": 900 if auxiliary else 0},
                        },
                    }
                    self.wfile.write(
                        ("data: " + json.dumps(frame) + "\n\ndata: [DONE]\n\n").encode()
                    )
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    # A completed response may lose its reader during explicit local restart.
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
        assert self.closed.is_set(), "network cancellation was not observed"


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/cache-warmer-install")
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    # The independent SDK-only author compiles the actual plugin source after host freezing.
    author = destination / "plugins" / library("author_cache_warmer")
    shutil.copy2(author_artifact("cache-warmer"), author)
    wrapper_library = destination / "plugins" / library("author_model_services")
    shutil.copy2(author_artifact("model-services"), wrapper_library)
    results: dict[str, Any] = {}
    for mode in ("idle", "streaming", "model", "resource", "branch", "projection", "off"):
        scratch = destination / mode
        scratch.mkdir()
        server = Server(scratch, mode)
        try:
            selected = copy.deepcopy(composition)
            target_path = scratch / "author-target.json"
            selected["packages"].append(
                package(
                    "model-services",
                    [
                        "eden.coding-provider.v1",
                        "eden.model-catalog.v1",
                        "eden.model-manager.v1",
                        "eden.credential-source.v1",
                    ],
                    str(wrapper_library),
                    target(),
                    {"target_path": str(target_path), "provider_wrapper": True},
                )
            )
            selected["runtime"] = {
                "scopes": {
                    "": {
                        "bindings": {
                            "eden.coding-provider.v1": {
                                "tail": "model-access",
                                "wrappers": ["model-services"],
                            },
                            "eden.model-manager.v1": {"tail": "model-services"},
                        }
                    }
                }
            }
            for item in selected["packages"]:
                name = item["descriptor"]["package"]
                if name == "cache-warmer":
                    item["library"] = str(author)
                    item["config"] = {
                        "mode": "streaming" if mode == "streaming" else "idle",
                        "interval_ms": 300,
                        "ttl_ms": 120000,
                        "safety_ms": 40000,
                        "timeout_ms": 30000,
                        "max_requests": 3,
                        "max_output_tokens": 1,
                    }
                if name == "model-access":
                    item["config"] = {
                        "catalog": {
                            "models": [
                                {
                                    "provider": "openai",
                                    "model": model,
                                    "api": "openai-completions",
                                    "base_url": server.base,
                                    "limits": {"context_window": 65536, "max_output_tokens": 777},
                                    "capabilities": {
                                        "tools": True,
                                        "images": False,
                                        "reasoning": False,
                                    },
                                    "source": {"kind": "author", "location": "controlled fixture"},
                                }
                                for model in ("warm-fixture", "warm-fixture-2")
                            ]
                        },
                        "credentials": {"path": str(scratch / "credentials.json")},
                    }
            model_config = next(
                item["config"]
                for item in selected["packages"]
                if item["descriptor"]["package"] == "model-access"
            )
            target_path.write_text(
                json.dumps(model_config["catalog"]["models"][0]), encoding="utf-8"
            )
            config = destination / f"{mode}.json"
            config.write_text(json.dumps(selected), encoding="utf-8")
            environment = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith(("OPENAI_", "ANTHROPIC_", "EDEN_API_"))
            }
            environment["OPENAI_API_KEY"] = "controlled-cache-key"
            completed = run(
                [example("cache_warm_probe"), config, scratch, mode],
                ROOT,
                env=environment,
                timeout=150,
            )
            results[mode] = json.loads(completed.stdout)
            results[mode]["http_requests"] = len(server.requests)
            results[mode]["auxiliary_requests"] = server.auxiliary
            assert results[mode]["wrapper_calls"] == server.foreground, (
                "wrapper invocation count differs from foreground HTTP attempts"
            )
            if mode == "streaming":
                assert server.auxiliary == 1, "streaming continued after settlement"
            elif mode == "idle":
                assert server.auxiliary >= 3, "idle did not survive two foreground turns"
            else:
                assert server.auxiliary == 1, "invalidated context produced another HTTP replay"
        finally:
            server.close()
    evidence = {
        "independent_actual_plugin": True,
        "scenarios": results,
        "limitation": "Controlled HTTP proves mechanisms only; no live provider cache-hit, cost or latency claim.",
    }
    (ROOT / "artifacts/cache-warmer-verification.json").write_text(
        json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(evidence))


if __name__ == "__main__":
    main()
