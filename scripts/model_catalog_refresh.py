"""Installed RPC startup, freshness and shutdown checks against a public-only catalog."""

import copy
import http.server
import json
import os
import pathlib
import queue
import subprocess
import tempfile
import threading
import time
from collections.abc import Callable
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT


def verify_background_catalog(host: pathlib.Path, composition: dict[str, Any]) -> dict[str, bool]:
    requests: list[str] = []
    errors: list[str] = []
    entered = threading.Event()
    closed = threading.Event()
    held = False

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            try:
                requests.append(self.path)
                assert self.path.startswith("/api/models/providers/"), self.path
                assert self.headers.get("Authorization") is None
                if held:
                    entered.set()
                    self.connection.settimeout(20)
                    assert self.connection.recv(1) == b"", "catalog transport survived shutdown"
                    closed.set()
                    return
                body = json.dumps(
                    [
                        {
                            "id": "background-fixture",
                            "api": "openai-responses",
                            "baseUrl": "http://localhost/v1",
                            "contextWindow": 65536,
                        }
                    ]
                ).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("ETag", "fixture-v1")
                self.send_header("Last-Modified", "Mon, 28 Sep 2026 00:00:00 GMT")
                self.end_headers()
                self.wfile.write(body)
            except BaseException as error:
                errors.append(repr(error))

        def do_POST(self) -> None:
            errors.append(f"background catalog attempted POST: {self.path}")
            self.send_error(500)

    server = FixtureHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    source = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="eden-background-catalog-") as directory:
            root = pathlib.Path(directory)
            global_dir = root / "global"
            global_dir.mkdir()
            (global_dir / "settings.json").write_text(
                json.dumps({"discover_skills": False, "discover_templates": False}),
                encoding="utf-8",
            )
            manifest = root / "composition.json"
            selected = copy.deepcopy(composition)
            # Relative native paths stay relative to the original installation, not this fixture.
            model_config: dict[str, Any] = {}
            for entry in selected["packages"]:
                entry["library"] = str((host.parent.parent / entry["library"]).resolve())
                if entry["descriptor"]["package"] == "model-access":
                    entry["config"] = {
                        "catalog": {"source": source},
                        "router": {"enabled": False},
                        "credentials": {
                            "providers": {
                                "openai": {
                                    "command": f'echo unexpected > "{root / "credential-command-ran"}"'
                                }
                            }
                        },
                    }
                    model_config = entry["config"]
            environment = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith(("OPENAI_", "AWS_", "GOOGLE_"))
            }

            def run(expect: str | None, hold: bool = False, offline_startup: bool = False) -> None:
                nonlocal held
                held = hold
                manifest.write_text(json.dumps(selected), encoding="utf-8")
                command = [
                    str(host),
                    "--composition",
                    str(manifest),
                    "--cwd",
                    str(root),
                    "--global-dir",
                    str(global_dir),
                    "rpc",
                ]
                if offline_startup:
                    command.insert(1, "--offline-startup")
                process = subprocess.Popen(
                    command,
                    env=environment,
                    text=True,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                frames: queue.Queue[dict[str, Any]] = queue.Queue()
                observed: list[dict[str, Any]] = []

                def collect() -> None:
                    assert process.stdout is not None
                    for line in process.stdout:
                        frame = json.loads(line)
                        observed.append(frame)
                        frames.put(frame)

                reader = threading.Thread(target=collect, daemon=True)
                reader.start()

                saved: list[dict[str, Any]] = []

                def receive(predicate: Callable[[dict[str, Any]], bool]) -> dict[str, Any]:
                    for index, frame in enumerate(saved):
                        if predicate(frame):
                            return saved.pop(index)
                    while True:
                        frame = frames.get(timeout=30)
                        assert frame.get("type") != "error", frame
                        if predicate(frame):
                            return frame
                        saved.append(frame)

                def send(identity: str, method: str) -> None:
                    assert process.stdin is not None
                    process.stdin.write(
                        json.dumps(
                            {
                                "version": 1,
                                "id": identity,
                                "session_id": session_id,
                                "method": method,
                                "params": {},
                            }
                        )
                        + "\n"
                    )
                    process.stdin.flush()

                try:
                    session_id = receive(lambda frame: frame.get("type") == "ready")["session_id"]
                    if hold:
                        assert entered.wait(10), "automatic refresh never connected"
                    send("state", "state")
                    state = receive(lambda frame: frame.get("id") == "state")
                    assert state["result"]["active_run"] is None, state
                    if expect is not None:
                        event = receive(
                            lambda frame: frame.get("event", {}).get("kind")
                            == "model_catalog_refreshed"
                        )
                        assert event["event"]["payload"]["status"] == expect, (
                            event,
                            requests,
                            errors,
                        )
                    send("stop", "shutdown")
                    assert process.stdin is not None
                    process.stdin.close()
                    assert process.wait(timeout=10) == 0
                    if hold:
                        assert closed.wait(2), "shutdown failed to close catalog connection"
                    reader.join(timeout=5)
                    assert not reader.is_alive()
                    if expect is None and not hold:
                        assert not any(
                            frame.get("event", {}).get("kind") == "job_settled"
                            for frame in observed
                        ), observed
                    assert process.stderr is not None
                    assert not process.stderr.read(), "unexpected RPC stderr"
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait()
                    for stream in [process.stdin, process.stdout, process.stderr]:
                        if stream is not None:
                            stream.close()

            def seed_cache() -> None:
                # Exercise one stale provider; multi-provider refresh is covered in catalog tests.
                bundled = json.loads(
                    (ROOT / "plugins/model-access/data/pi-models.json").read_text(encoding="utf-8")
                )
                entries = {}
                for provider, apis in bundled.items():
                    if provider == "radius":
                        continue
                    models = next(iter(apis.values()))
                    entries[provider] = {
                        "models": [next(iter(models.values()))],
                        "etag": "seed",
                        "updated_at": int(time.time()) - (14401 if provider == "openai" else 0),
                    }
                (global_dir / "model-catalog.json").write_text(
                    json.dumps({"sources": {source: entries}}), encoding="utf-8"
                )

            seed_cache()
            run("refreshed")
            count = len(requests)
            assert requests == ["/api/models/providers/openai"], requests
            cache = json.loads((global_dir / "model-catalog.json").read_text(encoding="utf-8"))
            assert cache["sources"][source]["openai"]["models"][0]["id"] == "background-fixture"
            run("fresh")
            assert len(requests) == count, "fresh startup fetched the catalog again"
            (global_dir / "model-catalog.json").unlink()
            model_config["catalog"]["offline"] = True
            run(None)
            assert len(requests) == count
            model_config["catalog"]["offline"] = False
            model_config["catalog"]["auto_refresh"] = False
            run(None)
            assert len(requests) == count
            model_config["catalog"]["auto_refresh"] = True
            run(None, offline_startup=True)
            assert len(requests) == count
            settings_path = global_dir / "settings.json"
            settings = json.loads(settings_path.read_text(encoding="utf-8"))
            settings["offline_startup"] = True
            settings_path.write_text(json.dumps(settings), encoding="utf-8")
            run(None)
            assert len(requests) == count
            settings["offline_startup"] = False
            settings_path.write_text(json.dumps(settings), encoding="utf-8")
            seed_cache()
            run(None, hold=True)
            assert requests == ["/api/models/providers/openai"] * 2, requests
            assert not (root / "credential-command-ran").exists()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    assert not errors, errors
    return {
        "startup_background": True,
        "foreground_idle": True,
        "durable_ttl": True,
        "offline": True,
        "offline_startup": True,
        "opt_out": True,
        "shutdown_transport_closed": True,
    }
