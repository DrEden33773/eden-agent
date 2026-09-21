#!/usr/bin/env python3
"""Installed controlled-router acceptance; this is not real-server evidence."""

import copy
import http.server
import json
import os
import pathlib
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
from typing import Any
from urllib.parse import parse_qs, urlsplit

from http_fixture import FixtureHTTPServer
from install import ROOT, library, package, target
from verification import author_artifact, example, installed, prepare

MANAGER = "eden.model-manager.v1"
SECRET = "G4-PRIVATE-router-canary"


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


class Router:
    """Remote mutation records and socket barriers distinguish cleanup from local exit."""

    def __init__(self) -> None:
        self.models: dict[str, dict[str, Any]] = {
            name: {
                "id": name,
                "source": source,
                "status": {"value": state, "failed": failed},
                "meta": {"n_ctx": 4096},
            }
            for name, source, state, failed in [
                ("shared", "cache", "loaded", False),
                ("sleeping", "cache", "sleeping", False),
                ("preset", "preset", "unloaded", False),
                ("ordinary", "cache", "unloaded", False),
                ("failed", "preset", "unloaded", True),
                ("pending", "cache", "downloading", False),
            ]
        }
        self.autoload = False
        self.max_instances = 0
        self.key: str | None = None
        self.mutations: list[tuple[str, str]] = []
        self.inference: list[dict[str, Any]] = []
        self.searches: list[dict[str, list[str]]] = []
        self.repo_searches: list[dict[str, list[str]]] = []
        self.inference_routes: list[tuple[str, str | None]] = []
        self.withhold_headers = False
        self.sse_entered = threading.Event()
        self.load_progress_sent = threading.Event()
        self.errors: list[str] = []
        self.lock = threading.Lock()
        self.condition = threading.Condition(self.lock)
        self.streams = 0
        self.stopping = threading.Event()
        self.post_entered = threading.Event()
        self.post_release = threading.Event()
        self.stream_closed = threading.Event()
        self.delay = False
        self.disconnected = False
        self.hold = False
        self.polls: dict[str, int] = {}
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def reply(self, value: object) -> None:
                data = json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def authorize(self) -> bool:
                if owner.disconnected:
                    self.connection.shutdown(socket.SHUT_RDWR)
                    return False
                assert self.headers.get("Authorization") == (
                    f"Bearer {owner.key}" if owner.key else None
                ), "router credential mismatch"
                return True

            def do_GET(self) -> None:
                try:
                    url = urlsplit(self.path)
                    if url.path == "/search":
                        assert self.headers.get("Authorization") is None
                        owner.searches.append(parse_qs(url.query))
                        self.reply([{"id": "test/tiny-GGUF", "downloads": 7, "gated": "manual"}])
                        return
                    if url.path == "/search/owner/repo":
                        assert self.headers.get("Authorization") is None
                        owner.repo_searches.append(parse_qs(url.query))
                        self.reply(
                            {
                                "id": "owner/repo",
                                "downloads": 19,
                                "gated": False,
                                "siblings": [
                                    {"rfilename": "tiny-Q4_K_M-00001-of-00002.gguf", "size": 100},
                                    {"rfilename": "tiny-Q4_K_M-00002-of-00002.gguf", "size": 200},
                                    {"rfilename": "tiny-Q8_0.gguf", "size": 400},
                                    {"rfilename": "mmproj-Q8_0.gguf", "size": 999},
                                    {"rfilename": "README.md", "size": 123},
                                ],
                            }
                        )
                        return
                    if not self.authorize():
                        return
                    if url.path == "/props":
                        self.reply(
                            {
                                "models_autoload": owner.autoload,
                                "max_instances": owner.max_instances,
                            }
                        )
                    elif url.path == "/models":
                        with owner.lock:
                            for name in list(owner.polls):
                                owner.polls[name] += 1
                                if (
                                    owner.polls[name] >= 3
                                    and not owner.hold
                                    and (name != "event-load" or owner.load_progress_sent.is_set())
                                ):
                                    current = owner.models[name]["status"]
                                    current["value"] = (
                                        "loaded" if current["value"] == "loading" else "unloaded"
                                    )
                                    del owner.polls[name]
                            data = copy.deepcopy(list(owner.models.values()))
                        self.reply({"data": data})
                    elif url.path == "/models/sse":
                        with owner.condition:
                            owner.streams += 1
                            owner.condition.notify_all()
                        owner.sse_entered.set()
                        try:
                            if not owner.withhold_headers:
                                self.send_response(200)
                                self.send_header("Content-Type", "text/event-stream")
                                self.end_headers()
                                self.wfile.write(b": connected\n\n")
                                self.wfile.flush()
                            while not owner.stopping.is_set() and not owner.disconnected:
                                if (
                                    not owner.withhold_headers
                                    and not owner.load_progress_sent.is_set()
                                    and ("/models/load", "event-load") in owner.mutations
                                ):
                                    event = {
                                        "event": "status_change",
                                        "model": "event-load",
                                        "data": {
                                            "status": "loading",
                                            "progress": {
                                                "stages": ["allocating", "loading"],
                                                "current": "loading",
                                                "value": 0.4,
                                            },
                                        },
                                    }
                                    self.wfile.write(
                                        ("data: " + json.dumps(event) + "\n\n").encode()
                                    )
                                    self.wfile.flush()
                                    owner.load_progress_sent.set()
                                readable, _, _ = select.select([self.connection], [], [], 0.1)
                                if readable and self.connection.recv(1) == b"":
                                    owner.stream_closed.set()
                                    break
                        finally:
                            with owner.condition:
                                owner.streams -= 1
                                owner.condition.notify_all()
                    else:
                        raise AssertionError(self.path)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except BaseException as error:
                    owner.errors.append(repr(error))

            def do_POST(self) -> None:
                try:
                    if not self.authorize():
                        return
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    if self.path in (
                        "/v1/chat/completions",
                        "/provider/v1/chat/completions",
                        "/explicit/v1/chat/completions",
                    ):
                        owner.inference.append(body)
                        owner.inference_routes.append(
                            (self.path, self.headers.get("x-route-proof"))
                        )
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        frame = {
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {"content": "router inference"},
                                    "finish_reason": "stop",
                                }
                            ]
                        }
                        self.wfile.write(
                            ("data: " + json.dumps(frame) + "\n\ndata: [DONE]\n\n").encode()
                        )
                        return
                    name = body["model"]
                    if name == "gated/denied:Q4":
                        self.send_response(403)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    if self.path in ("/models", "/models/load"):
                        owner.post_entered.set()
                        if owner.withhold_headers:
                            assert owner.sse_entered.wait(5), "SSE request never arrived"
                        if owner.delay:
                            assert owner.post_release.wait(15), "test did not release delayed POST"
                        with owner.lock:
                            owner.mutations.append((self.path, name))
                            owner.models[name] = {
                                "id": name,
                                "source": "cache",
                                "status": {
                                    "value": "loading"
                                    if self.path.endswith("load")
                                    else "downloading",
                                    "progress": {"file.gguf": {"done": 1, "total": 4}},
                                },
                                "meta": {"n_ctx": 4096},
                            }
                            owner.polls[name] = 0
                            if owner.withhold_headers:
                                owner.models[name]["status"]["value"] = "loaded"
                                owner.polls.pop(name)
                    elif self.path == "/models/unload":
                        with owner.lock:
                            owner.mutations.append((self.path, name))
                            owner.models[name]["status"] = {"value": "unloaded"}
                            owner.polls.pop(name, None)
                    else:
                        raise AssertionError(self.path)
                    self.reply({"success": True})
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except BaseException as error:
                    owner.errors.append(repr(error))

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.http.server_port}"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def no_watchers(self) -> None:
        with self.condition:
            assert self.condition.wait_for(lambda: self.streams == 0, timeout=10), "watcher leaked"
        assert not self.errors, self.errors

    def close(self) -> None:
        self.stopping.set()
        self.post_release.set()
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        self.no_watchers()


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/router-models-install")
    host = destination / "bin" / ("eden.exe" if os.name == "nt" else "eden")
    frozen = host.read_bytes()
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    lib = library("author_model_services")
    folder = destination / "plugins/model-services/0.1.0"
    folder.mkdir(parents=True)
    shutil.copy2(author_artifact("model-services"), folder / lib)
    results: dict[str, Any] = {"evidence": "installed-controlled-http"}
    with tempfile.TemporaryDirectory(prefix="eden-g4-router-") as temporary:
        scratch = pathlib.Path(temporary)
        caller, project, global_dir = [
            scratch / name for name in ("other cwd", "project", "global")
        ]
        for directory in (caller, project, global_dir):
            directory.mkdir()
        write(global_dir / "settings.json", {"discover_skills": False, "discover_templates": False})
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("LLAMA_", "OPENAI_", "ANTHROPIC_", "EDEN_API_"))
        }
        environment["EDEN_AGENT_DIR"] = str(global_dir)
        router = Router()
        try:
            config = destination / "g4.json"
            selected = copy.deepcopy(composition)
            for item in selected["packages"]:
                if item["descriptor"]["package"] == "model-access":
                    item["config"] = {
                        "router": {"url": router.base, "search_url": router.base + "/search"},
                        "credentials": {"path": str(global_dir / "keys.json")},
                    }
            write(config, selected)

            def command(args: list[str]) -> list[str]:
                return [
                    str(host),
                    "--composition",
                    str(config),
                    "--cwd",
                    str(project),
                    "--global-dir",
                    str(global_dir),
                    *args,
                ]

            def invoke(
                args: list[str],
                success: bool = True,
                error_contains: str | None = None,
                timeout: float = 45,
            ) -> list[dict[str, Any]]:
                done = subprocess.run(
                    command(args),
                    cwd=caller,
                    env=environment,
                    text=True,
                    capture_output=True,
                    timeout=timeout,
                )
                assert SECRET not in done.stdout + done.stderr
                assert "\x1b" not in done.stdout
                assert (done.returncode == 0) == success, (args, done.stdout, done.stderr)
                if error_contains is not None:
                    assert error_contains in done.stderr, done.stderr
                router.no_watchers()
                return [json.loads(line) for line in done.stdout.splitlines() if line.strip()]

            listing = invoke(["router", "list"])[-1]
            assert {m["id"] for m in listing["models"] if m["selectable"]} == {"shared", "sleeping"}
            catalog = invoke(["models", "list"])[-1]
            assert {
                m["target"]["model"]
                for m in catalog["models"]
                if m["target"]["provider"] == "llama.cpp"
            } == {"shared", "sleeping"}
            router.autoload = True
            listing = invoke(["router", "list"])[-1]
            assert {m["id"] for m in listing["models"] if m["selectable"]} == {
                "shared",
                "sleeping",
                "preset",
            }
            router.autoload = False
            results["projection_autoload_no_key"] = True
            environment["LLAMA_API_KEY"] = SECRET
            router.key = SECRET
            invoke(["router", "list"])
            search = invoke(["router", "search", "tiny model"])[-1]
            assert search["results"] == [
                {"id": "test/tiny-GGUF", "downloads": 7, "gated": True, "quants": []}
            ]
            assert router.searches[-1]["search"] == ["tiny model"]
            results["optional_key_search"] = True
            repo = invoke(["router", "search", "owner/repo"])[-1]["results"]
            assert len(repo) == 1 and repo[0]["id"] == "owner/repo", repo
            assert router.repo_searches == [{"blobs": ["true"]}]
            quants = {quant["name"]: quant for quant in repo[0]["quants"]}
            assert quants == {
                "Q4_K_M": {
                    "name": "Q4_K_M",
                    "bytes": 300,
                    "files": ["tiny-Q4_K_M-00001-of-00002.gguf", "tiny-Q4_K_M-00002-of-00002.gguf"],
                },
                "Q8_0": {"name": "Q8_0", "bytes": 400, "files": ["tiny-Q8_0.gguf"]},
            }, repo
            results["exact_repo_quant_shards_without_mmproj"] = True
            before = list(router.mutations)
            invoke(
                ["router", "download", "gated/denied:Q4"],
                success=False,
                error_contains="server's Hugging Face credentials",
            )
            assert router.mutations == before
            results["server_owned_download_permission"] = True
            for action, model in [("download", "test/tiny-GGUF:Q4_K_M"), ("load", "ordinary")]:
                events = invoke(["--json", "router", action, model])
                assert events[-1]["status"] == "completed", events
                progress = [
                    e["payload"] for e in events[:-1] if e.get("kind") == "model_management"
                ]
                assert any(e["status"] == "accepted" for e in progress), events
                assert any(e.get("progress") == 0.25 for e in progress), events
                assert router.models["shared"]["status"]["value"] == "loaded"
                results[action] = {"accepted_and_progress": True, "shared_kept": True}
            events = invoke(["--json", "router", "load", "event-load"])
            assert events[-1]["status"] == "completed", events
            assert any(
                event.get("kind") == "model_management"
                and event["payload"].get("status") == "loading"
                and event["payload"].get("progress") == 0.4
                for event in events
            ), events
            results["nested_loading_sse_progress"] = True
            router.withhold_headers = True
            router.sse_entered.clear()
            before = list(router.mutations)
            try:
                events = invoke(["--json", "router", "load", "fast-without-sse"], timeout=8)
                assert events[-1]["status"] == "completed", events
                assert router.mutations == before + [("/models/load", "fast-without-sse")]
                assert any(
                    event.get("kind") == "model_management"
                    and event["payload"].get("status") == "accepted"
                    for event in events
                )
            finally:
                router.withhold_headers = False
            results["withheld_sse_headers_fast_post_no_cleanup_unload"] = True

            # The manager observes the same identity; user routing must win in the catalog.
            for explicit in (False, True):
                override = copy.deepcopy(selected)
                for item in override["packages"]:
                    if item["descriptor"]["package"] != "model-access":
                        continue
                    item["config"]["catalog"] = {
                        "providers": {
                            "llama.cpp": {
                                "base_url": router.base + "/provider/v1",
                                "headers": {"x-route-proof": "provider"},
                                "compat": {"maxTokensField": "max_completion_tokens"},
                            }
                        }
                    }
                    if explicit:
                        item["config"]["catalog"]["models"] = [
                            {
                                "provider": "llama.cpp",
                                "model": "shared",
                                "api": "openai-completions",
                                "base_url": router.base + "/explicit/v1",
                                "headers": {"x-route-proof": "explicit"},
                                "limits": {"context_window": 16384, "max_output_tokens": 321},
                                "capabilities": {
                                    "tools": True,
                                    "images": False,
                                    "reasoning": False,
                                },
                                "compat": {"maxTokensField": "max_tokens"},
                                "source": {"kind": "explicit", "location": "configuration"},
                            }
                        ]
                write(config, override)
                catalog = invoke(["models", "list"])[-1]
                matched = [
                    entry["target"]
                    for entry in catalog["models"]
                    if entry["target"]["provider"] == "llama.cpp"
                    and entry["target"]["model"] == "shared"
                ]
                assert len(matched) == 1, matched
                effective = matched[0]
                route = "explicit" if explicit else "provider"
                field = "max_tokens" if explicit else "max_completion_tokens"
                limit = 321 if explicit else 4096
                assert effective["base_url"] == router.base + f"/{route}/v1", effective
                assert effective["compat"]["maxTokensField"] == field, effective
                assert effective["limits"]["max_output_tokens"] == limit, effective
                if explicit:
                    assert effective["limits"]["context_window"] == 16384, effective
                history = str(scratch / f"override-{route}.jsonl")
                invoke(["--session", history, "models", "select", "llama.cpp", "shared"])
                events = invoke(["--session", history, "--json", "Prove the selected route"])
                assert events[-1]["payload"]["outcome"]["status"] == "completed", events[-1]
                assert router.inference_routes[-1] == (f"/{route}/v1/chat/completions", route)
                assert router.inference[-1]["model"] == "shared"
                assert router.inference[-1][field] == limit, router.inference[-1]
            write(config, selected)
            results["manager_collision_provider_and_explicit_routing"] = True
            router.max_instances = 1
            before = list(router.mutations)
            invoke(["router", "load", "preset"], success=False)
            assert router.mutations == before
            invoke(["router", "load", "preset", "--unload-others"])
            assert router.models["shared"]["status"]["value"] == "unloaded"
            router.max_instances = 0
            invoke(["router", "unload", "preset"])
            invoke(["router", "cancel", "pending"])
            results["explicit_unload_others_and_cancel"] = True

            if os.name != "nt":
                for action in ("download", "load"):
                    router.delay = True
                    router.hold = True
                    router.post_entered.clear()
                    router.post_release.clear()
                    router.stream_closed.clear()
                    name = "cancel-" + action
                    process = subprocess.Popen(
                        command(["--json", "router", action, name]),
                        cwd=caller,
                        env=environment,
                        text=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                    try:
                        assert router.post_entered.wait(15), "POST never arrived"
                        process.send_signal(signal.SIGINT)
                        assert router.stream_closed.wait(10), "cancel did not drop watcher"
                        assert ("/models/unload", name) not in router.mutations
                        router.post_release.set()
                        stdout, stderr = process.communicate(timeout=40)
                        assert process.returncode != 0 and "cancel" in stderr.lower(), (
                            stdout,
                            stderr,
                        )
                        assert SECRET not in stdout + stderr
                        assert router.mutations[-2:] == [
                            ("/models" if action == "download" else "/models/load", name),
                            ("/models/unload", name),
                        ], router.mutations
                        assert router.models[name]["status"]["value"] == "unloaded"
                        router.no_watchers()
                    finally:
                        router.post_release.set()
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                    results["delayed_post_cancel_" + action] = True
                router.delay = False
                router.hold = True
                router.post_entered.clear()
                process = subprocess.Popen(
                    command(["--json", "router", "load", "disconnect"]),
                    cwd=caller,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    assert router.post_entered.wait(15)
                    router.disconnected = True
                    stdout, stderr = process.communicate(timeout=40)
                    assert process.returncode != 0
                    assert "RemoteStateUnknown" in stderr, (stdout, stderr)
                    router.no_watchers()
                finally:
                    router.disconnected = False
                    if process.poll() is None:
                        process.kill()
                        process.wait()
                before = list(router.mutations)
                invoke(["router", "reconnect"])
                assert router.mutations == before, "reconnect replayed a mutation"
                invoke(["router", "cancel", "disconnect"])
                results["disconnect_unknown_reconnect_no_replay"] = True
            else:
                results["signal_cancel"] = "unaccepted: requires Windows console injection"

            router.hold = True
            probe = subprocess.run(
                [str(example("router_probe")), str(config), "shutdown"],
                cwd=caller,
                env=environment,
                text=True,
                capture_output=True,
                timeout=50,
            )
            assert probe.returncode == 0, (probe.stdout, probe.stderr)
            assert router.models["sdk-shutdown"]["status"]["value"] == "unloaded"
            router.no_watchers()
            results["sdk_shutdown_remote_stop_stale_handle"] = json.loads(probe.stdout)

            target_path = scratch / "manager-target.json"
            write(
                target_path,
                {
                    "provider": "author-router",
                    "model": "old",
                    "api": "openai-completions",
                    "base_url": router.base + "/v1",
                    "limits": {"context_window": 8192, "max_output_tokens": 123},
                    "capabilities": {"tools": True, "images": False, "reasoning": False},
                    "source": {"kind": "author", "location": "independent manager"},
                },
            )
            author = package(
                "model-services",
                [
                    "eden.coding-provider.v1",
                    "eden.model-catalog.v1",
                    MANAGER,
                    "eden.credential-source.v1",
                ],
                f"plugins/model-services/0.1.0/{lib}",
                target(),
            )
            author["config"] = {"target_path": str(target_path)}
            replaced = copy.deepcopy(selected)
            replaced["packages"].append(author)
            replaced["roles"][MANAGER] = "model-services"
            write(config, replaced)
            router.key = None
            invoke(["router", "load", "author-new"])
            catalog = invoke(["models", "list"])[-1]
            assert any(m["target"]["model"] == "author-new" for m in catalog["models"])
            invoke(["models", "default", "author-router", "author-new"])
            events = invoke(["--json", "Prove manager selection through default inference"])
            assert events[-1]["payload"]["outcome"]["status"] == "completed", events[-1]
            assert router.inference[-1]["model"] == "author-new"
            assert router.inference[-1]["max_completion_tokens"] == 123
            results["independent_manager_default_catalog_provider"] = True
            assert host.read_bytes() == frozen
        finally:
            router.close()
    write(ROOT / "artifacts/router-models-verification.json", results)
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
