"""Installed OAuth entrypoint proof against controlled authorization and inference endpoints."""

import argparse
import base64
import hashlib
import http.server
import json
import os
import pathlib
import queue
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import urllib.request
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT
from verification import installed, prepare

CODE = "G3-private-authorization-code"
ACCESS = "G3-private-access-token"
REFRESH = "G3-private-refresh-token"
ROTATED = "G3-private-rotated-token"
MINTED = "G3-private-minted-key"
SECRETS = (CODE, ACCESS, REFRESH, ROTATED, MINTED)


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def no_secrets(text: str) -> None:
    assert not any(secret in text for secret in SECRETS), "secret leaked outside private contract"


class Server:
    def __init__(self) -> None:
        self.errors: list[str] = []
        self.grants: list[str] = []
        self.inferences: list[str] = []
        self.challenge = ""
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                try:
                    raw = self.rfile.read(int(self.headers["Content-Length"])).decode()
                    body = (
                        json.loads(raw)
                        if "application/json" in self.headers.get("Content-Type", "")
                        else dict(urllib.parse.parse_qsl(raw))
                    )
                    if self.path in ("/token", "/mint"):
                        grant = body.get("grant_type", "mint")
                        if grant == "refresh_token":
                            assert body["refresh_token"] == REFRESH, "wrong refresh grant"
                            token = ROTATED
                        else:
                            assert body["code"] == CODE, "wrong private input"
                            challenge = (
                                base64.urlsafe_b64encode(
                                    hashlib.sha256(body["code_verifier"].encode()).digest()
                                )
                                .decode()
                                .rstrip("=")
                            )
                            assert challenge == owner.challenge, "PKCE mismatch"
                            token = ACCESS
                        owner.grants.append(grant)
                        reply = (
                            {"key": MINTED}
                            if self.path == "/mint"
                            else {
                                "access_token": token,
                                "refresh_token": REFRESH,
                                "expires_in": 3600,
                            }
                        )
                        payload = json.dumps(reply).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(payload)))
                        self.end_headers()
                        self.wfile.write(payload)
                    else:
                        assert self.path == "/v1/messages", self.path
                        assert self.headers.get("Authorization") == f"Bearer {ROTATED}", (
                            "inference did not receive refreshed OAuth token"
                        )
                        assert self.headers.get("x-api-key") is None, "OAuth emitted API-key auth"
                        assert "oauth-2025-04-20" in self.headers.get("anthropic-beta", "")
                        assert body["model"] == "oauth-fixture"
                        owner.inferences.append(body["model"])
                        events = [
                            {"type": "message_start", "message": {"usage": {"input_tokens": 3}}},
                            {
                                "type": "content_block_start",
                                "index": 0,
                                "content_block": {"type": "text", "text": ""},
                            },
                            {
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {
                                    "type": "text_delta",
                                    "text": "refreshed account accepted",
                                },
                            },
                            {"type": "content_block_stop", "index": 0},
                            {
                                "type": "message_delta",
                                "delta": {"stop_reason": "end_turn"},
                                "usage": {"output_tokens": 4},
                            },
                            {"type": "message_stop"},
                        ]
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        for event in events:
                            self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                    self.wfile.flush()
                except BaseException as error:
                    # Assertion messages never embed private request bodies or headers.
                    owner.errors.append(type(error).__name__ + ": " + str(error))

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.http.server_port}"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


class Login:
    """Keep stdin open through settlement to detect an orphaned blocking helper."""

    def __init__(self, command: list[str], caller: pathlib.Path, env: dict[str, str]) -> None:
        self.process = subprocess.Popen(
            command,
            cwd=caller,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        assert self.process.stdin is not None
        self.input = self.process.stdin
        self.lines: list[str] = []
        self.ready: queue.Queue[str] = queue.Queue()

        def consume() -> None:
            assert self.process.stderr is not None
            for line in self.process.stderr:
                self.lines.append(line)
                self.ready.put(line)
            self.ready.put("")

        self.reader = threading.Thread(target=consume, daemon=True)
        self.reader.start()

    def challenge(self) -> dict[str, str]:
        while line := self.ready.get(timeout=30):
            if line.startswith("Open: "):
                return dict(urllib.parse.parse_qsl(urllib.parse.urlparse(line[6:].strip()).query))
        raise AssertionError("login exited before showing an authorization challenge")

    def input_ready(self) -> None:
        while line := self.ready.get(timeout=30):
            if line.startswith("Paste the authorization"):
                return
        raise AssertionError("login exited before accepting private input")

    def finish(self, success: bool) -> str:
        self.process.wait(timeout=30)
        self.reader.join(timeout=5)
        assert not self.reader.is_alive(), "CLI did not close stderr"
        assert self.process.stdout is not None
        stdout = self.process.stdout.read()
        stderr = "".join(self.lines)
        no_secrets(stdout + stderr)
        assert (self.process.returncode == 0) == success, (stdout, stderr)
        # Unlike communicate(), waiting above preserves stdin. An orphaned helper
        # would keep this pipe readable and make the write succeed.
        try:
            self.input.write("reader must already be gone\n")
            self.input.flush()
        except BrokenPipeError:
            pass
        else:
            raise AssertionError("login left a private-input reader alive")
        return stdout

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=10)
        for stream in (self.input, self.process.stdout, self.process.stderr):
            if stream is not None:
                try:
                    stream.close()
                except BrokenPipeError:
                    pass
        self.reader.join(timeout=5)


def verify(destination: pathlib.Path) -> dict[str, Any]:
    host = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
    composition = json.loads((destination / "composition.json").read_text(encoding="utf-8"))
    results: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="eden-oauth-installed-") as temporary:
        scratch = pathlib.Path(temporary)
        caller, project, global_dir = (scratch / name for name in ("caller", "project", "global"))
        for folder in (caller, project, global_dir):
            folder.mkdir()
        write(global_dir / "settings.json", {"discover_skills": False, "discover_templates": False})
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("ANTHROPIC_", "OPENROUTER_", "EDEN_API_", "OPENAI_"))
        }
        environment["EDEN_AGENT_DIR"] = str(global_dir)
        server = Server()
        try:
            config = destination / "oauth-fixture.json"
            for package in composition["packages"]:
                if package["descriptor"]["package"] == "model-access":
                    package["config"] = {
                        "catalog": {
                            "offline": True,
                            "models": [
                                {
                                    "provider": "anthropic",
                                    "model": "oauth-fixture",
                                    "api": "anthropic-messages",
                                    "base_url": server.base + "/v1",
                                    "limits": {"context_window": 65536, "max_output_tokens": 777},
                                    "capabilities": {
                                        "tools": True,
                                        "images": False,
                                        "reasoning": False,
                                    },
                                    "source": {
                                        "kind": "custom",
                                        "location": "controlled OAuth fixture",
                                    },
                                }
                            ],
                        },
                        "credentials": {
                            "path": str(global_dir / "credentials.json"),
                            "oauth": {
                                provider: {
                                    "client_id": "admitted-fixture-client",
                                    "authorization_url": server.base + "/authorize",
                                    "token_url": server.base + endpoint,
                                    "redirect_uri": "http://127.0.0.1:0/callback",
                                }
                                for provider, endpoint in (
                                    ("anthropic", "/token"),
                                    ("openrouter", "/mint"),
                                )
                            },
                        },
                    }
            write(config, composition)
            command = [
                str(item)
                for item in (
                    host,
                    "--composition",
                    config,
                    "--cwd",
                    project,
                    "--global-dir",
                    global_dir,
                )
            ]

            def invoke(args: list[str]) -> str:
                completed = subprocess.run(
                    command + args,
                    cwd=caller,
                    env=environment,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=60,
                )
                no_secrets(completed.stdout + completed.stderr)
                if completed.returncode:
                    for stream, contents in (
                        ("stdout", completed.stdout),
                        ("stderr", completed.stderr),
                    ):
                        (ROOT / "artifacts" / f"oauth-models-failure.{stream}.log").write_text(
                            contents, encoding="utf-8"
                        )
                assert completed.returncode == 0, (
                    completed.stdout.splitlines()[-3:],
                    completed.stderr,
                    server.errors,
                )
                return completed.stdout

            for mode, provider in (
                ("callback", "anthropic"),
                ("manual", "openrouter"),
                ("cancel", "anthropic"),
            ):
                if mode == "cancel" and os.name == "nt":
                    results[mode] = "unaccepted: CLI Ctrl-C injection requires a Windows console"
                    continue
                login = Login(command + ["auth", "login", provider], caller, environment)
                before = len(server.grants)
                try:
                    challenge = login.challenge()
                    server.challenge = challenge["code_challenge"]
                    redirect = challenge.get("redirect_uri", challenge.get("callback_url", ""))
                    login.input_ready()
                    if mode == "callback":
                        callback = (
                            redirect
                            + "?"
                            + urllib.parse.urlencode({"code": CODE, "state": challenge["state"]})
                        )
                        with urllib.request.urlopen(callback, timeout=10) as response:
                            assert response.status == 200
                    elif mode == "manual":
                        login.input.write(CODE + "\n")
                        login.input.flush()
                    else:
                        login.process.send_signal(signal.SIGINT)
                    try:
                        output = login.finish(mode != "cancel")
                    except subprocess.TimeoutExpired as error:
                        raise AssertionError(
                            f"{mode} login failed to settle; grants={server.grants}"
                        ) from error
                    if mode != "cancel":
                        assert json.loads(output)["status"] == "completed"
                        assert len(server.grants) == before + 1, server.errors
                    else:
                        assert len(server.grants) == before, "cancel exchanged a token"
                    endpoint = urllib.parse.urlparse(redirect)
                    with socket.socket() as probe:
                        probe.settimeout(2)
                        assert probe.connect_ex(("127.0.0.1", endpoint.port or 80)) != 0, (
                            "callback listener survived settled CLI"
                        )
                    results[mode] = {"private_reader_reaped": True, "callback_closed": True}
                finally:
                    login.close()
            assert json.loads(invoke(["auth", "refresh", "anthropic"]))["status"] == "completed"
            assert server.grants == ["authorization_code", "mint", "refresh_token"]
            history = scratch / "oauth.jsonl"
            invoke(["--session", str(history), "models", "select", "anthropic", "oauth-fixture"])
            for prompt in ("Confirm refreshed account access", "Continue after reopening"):
                events = [
                    json.loads(line)
                    for line in invoke(["--session", str(history), "--json", prompt]).splitlines()
                ]
                assert events[-1]["kind"] == "settled"
                assert events[-1]["payload"]["outcome"]["status"] == "completed", events[-1]
                no_secrets(history.read_text(encoding="utf-8"))
            assert len(server.inferences) == 2, server.errors
            results["refresh_then_inference_and_reopen"] = True
            assert json.loads(invoke(["auth", "logout", "anthropic"]))["status"] == "logged_out"
            stored = json.loads((global_dir / "credentials.json").read_text(encoding="utf-8"))
            assert "anthropic" not in stored["oauth"]
            assert "openrouter" in stored["oauth"], "logout removed another provider"
            results["logout_preserves_other_account"] = True
        finally:
            server.close()
    return results


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=pathlib.Path, help="Use an already assembled layout")
    args = parser.parse_args()
    if args.installation is None:
        prepare()
        destination = installed(ROOT / "artifacts/oauth-models-install")
    else:
        destination = args.installation.resolve()
    results = verify(destination)
    write(
        ROOT / "artifacts/oauth-models-verification.json",
        {"status": "passed", "scenarios": results},
    )
    print("Installed OAuth model access verification passed")
