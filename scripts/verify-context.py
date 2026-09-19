#!/usr/bin/env python3
"""Installed default-context, provider recovery, and durable queue acceptance."""

import base64
import copy
import hashlib
import http.server
import json
import os
import pathlib
import platform
import subprocess
import sys
import tempfile
import threading
from collections.abc import Callable, Sequence
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT, Composition, target
from verification import example, installed, prepare, short_retries
from verification import run as measured_run


def run(
    args: Sequence[str | pathlib.Path], cwd: pathlib.Path, check: bool = True
) -> subprocess.CompletedProcess[str]:
    return measured_run(args, cwd, check=check, timeout=180)


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def records(path: pathlib.Path) -> list[dict[str, Any]]:
    result = []
    for line in path.read_text(encoding="utf-8").splitlines():
        value = json.loads(line)
        result.extend(value.get("transaction", [value]))
    return result


def answer(text: str) -> list[dict[str, Any]]:
    return [
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }
    ]


def complete(output: list[dict[str, Any]], total: int = 15) -> dict[str, Any]:
    return {
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": output,
            "usage": {
                "input_tokens": total - 5,
                "output_tokens": 5,
                "total_tokens": total,
            },
        },
    }


def effect() -> list[dict[str, Any]]:
    return [
        {
            "type": "function_call",
            "call_id": "effect",
            "name": "bash",
            "arguments": json.dumps({"command": "printf x >> execution-count.txt"}),
        }
    ]


def summary(body: dict[str, Any]) -> bool:
    return body["tools"] == []


class Server:
    """The response callback owns faults; gates announce readiness before waiting."""

    def __init__(
        self,
        respond: Callable[[dict[str, Any], int], tuple[int, dict[str, Any]]],
        gate: int | None = None,
    ) -> None:
        self.requests: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.markers: list[dict[str, str | int]] = []
        self.release = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                owner.markers.append({"path": self.path, "requests": len(owner.requests)})
                self.send_response(200)
                self.end_headers()

            def do_POST(self) -> None:
                try:
                    assert self.headers["Authorization"] == "Bearer context-verifier-key"
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    assert body["model"] == "controlled-model" and body["stream"] is True
                    index = len(owner.requests)
                    owner.requests.append(body)
                    status, payload = respond(body, index)
                    self.send_response(status)
                    self.send_header(
                        "Content-Type",
                        "text/event-stream" if status == 200 else "application/json",
                    )
                    self.end_headers()
                    if status == 200:
                        if gate == index:
                            delta = {
                                "type": "response.output_text.delta",
                                "delta": f"gate-{index}",
                            }
                            self.wfile.write(("data: " + json.dumps(delta) + "\n\n").encode())
                            self.wfile.flush()
                            assert owner.release.wait(90), (
                                "cancelled client did not finish gate scenario"
                            )
                        self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
                    else:
                        self.wfile.write(json.dumps(payload).encode())
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
    destination: pathlib.Path,
    composition: Composition,
    server: Server,
    name: str,
    window: int = 1048576,
    allowance: int = 393216,
    keep_recent: int | None = None,
) -> pathlib.Path:
    selected = copy.deepcopy(composition)
    for package in selected["packages"]:
        if package["descriptor"]["package"] == "coding" and keep_recent is not None:
            package["config"] = {"compaction": {"keep_recent_tokens": keep_recent}}
        if package["descriptor"]["package"] == "model-access":
            package["config"] = {
                "endpoint": f"http://{server.address}/responses",
                "model": "controlled-model",
                "api_key_env": "EDEN_CONTEXT_KEY",
                "context_window": window,
                "max_output_tokens": allowance,
            }
    # See verify-coding.py: the retry count is asserted, the delay never is.
    short_retries(selected)
    path = destination / f"{name}.json"
    write(path, selected)
    return path


def main() -> None:
    os.environ["EDEN_CONTEXT_KEY"] = "context-verifier-key"
    prepare()
    artifacts = ROOT / "artifacts"
    artifacts.mkdir(exist_ok=True)
    destination = installed(artifacts / "context-install")
    suffix = ".exe" if sys.platform == "win32" else ""
    host = destination / "bin" / ("eden" + suffix)
    probe = example("context_probe")
    fixed_host = host.read_bytes()
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-context-") as temp:
        scratch = pathlib.Path(temp)
        caller = scratch / "unrelated caller 工作目录"
        caller.mkdir()
        project = scratch / "project"
        project.mkdir()

        def task(
            config: pathlib.Path,
            history: pathlib.Path,
            prompt: str = "Continue task",
            extras: Sequence[str | pathlib.Path] = (),
        ) -> subprocess.CompletedProcess[str]:
            return run(
                [
                    host,
                    "--composition",
                    config,
                    "--cwd",
                    project,
                    "--session",
                    history,
                    "--json",
                    *extras,
                    prompt,
                ],
                caller,
            )

        def command(
            config: pathlib.Path, history: pathlib.Path, name: str, *options: str
        ) -> subprocess.CompletedProcess[str]:
            return run(
                [host, "session", name, history, "--composition", config, *options],
                caller,
            )

        # A short transcript has no compactable prefix, so a manual compaction must
        # keep it whole and issue no summary call at all.
        short_history = scratch / "short-recent.jsonl"
        short = Server(lambda *_: (200, complete(answer("SHORT-RECENT-ANSWER"))))
        try:
            config = configure(destination, composition, short, "short-recent")
            task(config, short_history, "SHORT-RECENT-GOAL")
            command(config, short_history, "compact")
            assert len(short.requests) == 1, (
                "short manual compaction must not discard recent context"
            )
            assert not any(r["kind"] == "compaction" for r in records(short_history))
            task(config, short_history, "Continue recent context")
            assert "SHORT-RECENT-GOAL" in json.dumps(short.requests[-1]["input"])
            assert "SHORT-RECENT-ANSWER" in json.dumps(short.requests[-1]["input"])
            results["default_recent_retention"] = {
                "keep_recent_tokens": 20000,
                "short_compaction_requests": 0,
                "recent_goal_and_answer_in_provider_input": True,
            }
        finally:
            short.close()

        # Real default summary calls, repeated projection, and attachment reinclusion.
        history = scratch / "compaction.jsonl"
        image = scratch / "pixel.png"
        image.write_bytes(
            base64.b64decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jR5kAAAAASUVORK5CYII="
            )
        )
        summary_count = [0]

        def repeated(body: dict[str, Any], _: int) -> tuple[int, dict[str, Any]]:
            if summary(body):
                summary_count[0] += 1
                assert body["max_output_tokens"] in [8192, 13107]
                if summary_count[0] == 2:
                    assert "summary-marker-1" in json.dumps(body["input"])
                return 200, complete(
                    answer(
                        f"summary-marker-{summary_count[0]}: preserve requested goal and image reference"
                    )
                )
            return 200, complete(answer("ordinary response"))

        server = Server(repeated)
        try:
            config = configure(destination, composition, server, "repeated", keep_recent=1)
            task(
                config,
                history,
                "Remember attached image and ORIGINAL-GOAL",
                ["--image", image],
            )
            original = records(history)
            image_record = next(
                r["sequence"]
                for r in original
                if r["kind"] == "message"
                and any(b.get("type") == "image" for b in r["payload"].get("content", []))
            )
            command(config, history, "compact")
            task(config, history, "First continued task")
            first_projection = server.requests[-1]["input"]
            assert "summary-marker-1" in json.dumps(first_projection)
            assert "ordinary response" in json.dumps(first_projection), (
                "recent response must survive manual compaction"
            )
            command(config, history, "compact")
            task(config, history, "Second continued task")
            command(config, history, "include-attachment", "--at", str(image_record))
            task(config, history, "Inspect reintroduced image")
            assert any(
                block.get("type") == "input_image"
                for item in server.requests[-1]["input"]
                for block in item.get("content", [])
            )
            assert records(history)[: len(original)] == original
            assert summary_count[0] == 2
            results["repeated_manual_compaction"] = {
                "summary_calls": 2,
                "summary_allowances": [
                    body["max_output_tokens"] for body in server.requests if summary(body)
                ],
                "fixture_keep_recent_tokens": 1,
                "raw_prefix_preserved": True,
                "recent_response_preserved": True,
                "summary_in_actual_provider_input": True,
                "explicit_attachment_reincluded": True,
            }
        finally:
            server.close()

        # A bounded summary that truncates or is cancelled cannot replace context.
        for failure_mode in ["incomplete_summary", "compact_cancel"]:
            failed_history = scratch / f"{failure_mode}.jsonl"
            seeded = Server(lambda *_: (200, complete(answer("preserve-summary-source"))))
            try:
                config = configure(destination, composition, seeded, f"seed-{failure_mode}")
                task(config, failed_history, "SUMMARY-SOURCE-GOAL")
                original = records(failed_history)
            finally:
                seeded.close()
            failure = {
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"output_tokens": 10},
                },
            }
            server = Server(
                lambda *_, failure=failure: (200, failure),
                gate=0 if failure_mode == "compact_cancel" else None,
            )
            try:
                config = configure(destination, composition, server, failure_mode, keep_recent=1)
                if failure_mode == "incomplete_summary":
                    failed = run(
                        [
                            host,
                            "session",
                            "compact",
                            failed_history,
                            "--composition",
                            config,
                        ],
                        caller,
                        check=False,
                    )
                    assert failed.returncode != 0 and "ProviderFailure" in failed.stderr
                else:
                    report = json.loads(
                        run(
                            [
                                probe,
                                config,
                                project,
                                failed_history,
                                "compact_cancel",
                                server.address,
                            ],
                            caller,
                        ).stdout
                    )
                    assert report["terminal"]["outcome"]["status"] == "cancelled"
                assert len(server.requests) == 1 and summary(server.requests[0])
                durable = records(failed_history)
                assert durable[: len(original)] == original
                assert not any(r["kind"] == "compaction" for r in durable)
                results[failure_mode] = {
                    "requests": 1,
                    "original_prefix_preserved": True,
                    "no_compaction_committed": True,
                }
            finally:
                server.close()

        # Seed ordinary complete history, then independently lower the declared window.
        threshold_history = scratch / "threshold.jsonl"
        server = Server(lambda *_: (200, complete(answer("long context " + "a" * 100000))))
        try:
            config = configure(destination, composition, server, "threshold-seed")
            task(config, threshold_history, "First long turn")
            task(config, threshold_history, "Second long turn")
        finally:
            server.close()
        server = Server(
            lambda body, _: (
                200,
                complete(answer("threshold-summary" if summary(body) else "after threshold")),
            )
        )
        try:
            config = configure(destination, composition, server, "threshold", window=50000)
            task(config, threshold_history, "Trigger threshold")
            assert summary(server.requests[0]) and not summary(server.requests[-1])
            assert "threshold-summary" in json.dumps(server.requests[-1]["input"])
            assert any(r["kind"] == "compaction" for r in records(threshold_history))
            results["threshold"] = {
                "configured_window": 50000,
                "requests": len(server.requests),
                "summary_in_provider_input": True,
            }
        finally:
            server.close()

        for mode in ["overflow", "length", "retry_after_tool", "usage_overflow"]:
            recovery_history = scratch / f"{mode}.jsonl"
            count_file = project / "execution-count.txt"
            if count_file.exists():
                count_file.unlink()
            if mode in ["overflow", "length", "usage_overflow"]:
                seed = Server(
                    lambda *_: (
                        200,
                        complete(answer("prior task context " + "h" * 100000)),
                    )
                )
                try:
                    config = configure(destination, composition, seed, f"seed-{mode}")
                    task(config, recovery_history, "Earlier completed task to compact")
                finally:
                    seed.close()

            def respond(
                body: dict[str, Any],
                index: int,
                mode: str = mode,
                count_file: pathlib.Path = count_file,
            ) -> tuple[int, dict[str, Any]]:
                if mode == "overflow" and index == 0:
                    return 400, {
                        "error": {
                            "code": "context_length_exceeded",
                            "message": "controlled overflow",
                        }
                    }
                if mode == "length" and index == 0:
                    return 200, {
                        "type": "response.incomplete",
                        "response": {
                            "status": "incomplete",
                            "incomplete_details": {"reason": "max_output_tokens"},
                            "usage": {"output_tokens": 10},
                            "output": effect(),
                        },
                    }
                if mode == "retry_after_tool":
                    if index == 0:
                        return 200, complete(effect())
                    assert count_file.read_text() == "x", "historical tool executed again"
                    assert any(
                        item.get("call_id") == "effect" and item["type"] == "function_call_output"
                        for item in body["input"]
                    )
                    if index == 1:
                        return 503, {"error": {"code": "server_error"}}
                return 200, complete(
                    answer("recovery-summary" if summary(body) else "recovered"),
                    total=1100000 if mode == "usage_overflow" and index == 0 else 15,
                )

            server = Server(respond)
            try:
                config = configure(destination, composition, server, mode)
                data = task(config, recovery_history)
                durable = records(recovery_history)
                if mode in ["overflow", "length"]:
                    assert len(server.requests) == 3 and summary(server.requests[1])
                    assert "recovery-summary" in json.dumps(server.requests[2]["input"])
                    assert sum(r["kind"] == "compaction" for r in durable) == 1
                    assert not any(r["kind"] == "tool_intent" for r in durable)
                    assert not count_file.exists()
                elif mode == "retry_after_tool":
                    assert len(server.requests) == 3 and not any(
                        summary(body) for body in server.requests
                    )
                    assert server.requests[1]["input"] == server.requests[2]["input"]
                    assert sum(r["kind"] == "tool_intent" for r in durable) == 1
                    assert sum(r["kind"] == "tool_result" for r in durable) == 1
                    assert count_file.read_text() == "x"
                else:
                    assert len(server.requests) == 2 and summary(server.requests[1])
                    assert sum(r["kind"] == "compaction" for r in durable) == 1
                results[mode] = {
                    "requests": len(server.requests),
                    "compactions": sum(r["kind"] == "compaction" for r in durable),
                    "tool_intents": sum(r["kind"] == "tool_intent" for r in durable),
                    "terminal": json.loads(data.stdout.splitlines()[-1])["payload"],
                }
            finally:
                server.close()

        for mode in ["one", "all", "cancel_pre", "cancel_post"]:
            queue_history = scratch / f"queue-{mode}.jsonl"
            count_file = project / "execution-count.txt"
            if count_file.exists():
                count_file.unlink()
            server = Server(
                lambda _, index, mode=mode: (
                    200,
                    complete(
                        effect()
                        if mode == "cancel_post" and index == 0
                        else answer("queue response")
                    ),
                ),
                gate=0 if mode == "cancel_pre" else 1 if mode == "cancel_post" else None,
            )
            try:
                config = configure(destination, composition, server, f"queue-{mode}")
                result = json.loads(
                    run(
                        [probe, config, project, queue_history, mode, server.address],
                        caller,
                    ).stdout
                )
                durable = records(queue_history)
                ids = result["ids"]
                if mode in ["one", "all"]:
                    assert len(server.requests) == (3 if mode == "one" else 2)
                    consumed = [r for r in durable if r["kind"] == "queue_consumed"]
                    assert sorted(r["payload"]["id"] for r in consumed) == sorted(ids)
                    admissions = [
                        r["payload"]["queue_ids"] for r in durable if r["kind"] == "model_request"
                    ]
                    expected = (
                        [[ids[0]], [ids[1], ids[2]], [ids[3]]]
                        if mode == "one"
                        else [ids[:2], ids[2:]]
                    )
                    assert [sorted(group) for group in admissions] == [
                        sorted(group) for group in expected
                    ]
                    for line in queue_history.read_text(encoding="utf-8").splitlines():
                        transaction = json.loads(line)["transaction"]
                        for consumed_record in (
                            r for r in transaction if r["kind"] == "queue_consumed"
                        ):
                            assert any(
                                r["kind"] == "model_response"
                                and r["payload"]["request_id"]
                                == consumed_record["payload"]["request_id"]
                                for r in transaction
                            )
                    request_ids = {
                        r["payload"]["request_id"] for r in durable if r["kind"] == "model_request"
                    }
                    assert all(r["payload"]["request_id"] in request_ids for r in consumed)
                    final_input = json.dumps(server.requests[-1]["input"])
                    assert all(
                        label in final_input
                        for label in [
                            "steer-one",
                            "steer-two",
                            "follow-one",
                            "follow-two",
                        ]
                    )
                    first_input = json.dumps(server.requests[0]["input"])
                    assert ("steer-two" in first_input) is (mode == "all")
                elif mode == "cancel_pre":
                    assert len(server.requests) == 1
                    assert not any(
                        r["kind"] in ["queue_consumed", "model_response", "tool_intent"]
                        for r in durable
                    )
                    returned = [
                        r["payload"]["id"] for r in durable if r["kind"] == "queue_returned"
                    ]
                    assert returned == ids
                else:
                    assert len(server.requests) == 2
                    assert count_file.read_text() == "x"
                    assert sum(r["kind"] == "queue_consumed" for r in durable) == 1
                    assert not any(r["kind"] == "queue_returned" for r in durable)
                results[mode] = {
                    "requests": len(server.requests),
                    "ids": ids,
                    "terminal": result["terminal"],
                }
            finally:
                server.close()
            if mode.startswith("cancel_"):
                reopened = Server(lambda *_: (200, complete(answer("explicit continuation"))))
                try:
                    config = configure(destination, composition, reopened, f"reopen-{mode}")
                    result = json.loads(
                        run(
                            [
                                probe,
                                config,
                                project,
                                queue_history,
                                "resume",
                                reopened.address,
                            ],
                            caller,
                        ).stdout
                    )
                    assert reopened.markers == [{"path": "/opened", "requests": 0}]
                    assert result["ids"] == (ids if mode == "cancel_pre" else [])
                    assert len(reopened.requests) == 1
                    assert "queued-causal-input" in json.dumps(reopened.requests[0]["input"])
                    if mode == "cancel_post":
                        assert count_file.read_text() == "x"
                        assert sum(r["kind"] == "tool_intent" for r in records(queue_history)) == 1
                    results[mode]["new_process_reopen"] = {
                        "before_explicit_continue_requests": 0,
                        "restored_ids": result["ids"],
                        "requests": 1,
                        "old_tools_not_replayed": True,
                    }
                finally:
                    reopened.close()
        assert host.read_bytes() == fixed_host
    evidence = {
        "platform": platform.platform(),
        "target": target(),
        "commit": run(["git", "rev-parse", "HEAD"], ROOT).stdout.strip(),
        "source_dirty": bool(run(["git", "status", "--porcelain"], ROOT).stdout.strip()),
        "host_sha256": hashlib.sha256(fixed_host).hexdigest(),
        "host_unchanged": True,
        "installed_library_sha256": {
            str(path.relative_to(destination)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted((destination / "plugins").rglob("*"))
            if path.is_file()
        },
        "provider_evidence": "controlled HTTP using installed default native plugins",
        "scenarios": results,
    }
    write(artifacts / "context-verification.json", evidence)
    print(json.dumps(evidence, ensure_ascii=True))


if __name__ == "__main__":
    main()
