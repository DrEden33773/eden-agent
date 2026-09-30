#!/usr/bin/env python3
"""Native adapter and real PTY regressions for S4 management, using isolated fixture services."""

import argparse
import http.server
import importlib.util
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

import verify as probe

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
OAUTH_SPEC = importlib.util.spec_from_file_location(
    "workflow_oauth", ROOT / "scripts/oauth_models.py"
)
assert OAUTH_SPEC and OAUTH_SPEC.loader
oauth = importlib.util.module_from_spec(OAUTH_SPEC)
OAUTH_SPEC.loader.exec_module(oauth)
CODE, SECRETS, OAuthServer = oauth.CODE, oauth.SECRETS, oauth.Server
KEY = "S4_PRIVATE_API_KEY_CANARY"
CONFIG_KEY = "S4_PRIVATE_CONFIG_CANARY"
DEVICE_SECRET = "S4_PRIVATE_DEVICE_CODE"
DEVICE_ACCESS = "S4_PRIVATE_DEVICE_ACCESS"


class DeviceServer:
    def __init__(self):
        self.allow = threading.Event()
        self.errors = []
        self.polls = 0
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format, *args):
                pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers["Content-Length"])).decode()
                body = (
                    json.loads(raw)
                    if "application/json" in self.headers.get("Content-Type", "")
                    else dict(urllib.parse.parse_qsl(raw))
                )
                if self.path == "/device":
                    status, result = (
                        200,
                        {
                            "device_code": DEVICE_SECRET,
                            "user_code": "DEVICE-USER-CODE",
                            "verification_uri": owner.base + "/authorize",
                            "expires_in": 60,
                            "interval": 1,
                        },
                    )
                else:
                    owner.polls += 1
                    if body.get("device_code") != DEVICE_SECRET:
                        owner.errors.append("wrong device credential")
                    status, result = (
                        (
                            200,
                            {
                                "access_token": DEVICE_ACCESS,
                                "refresh_token": "device-fixture-refresh",
                                "expires_in": 3600,
                            },
                        )
                        if owner.allow.is_set()
                        else (400, {"error": "authorization_pending"})
                    )
                encoded = json.dumps(result).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.http.server_port}"
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


class Fixture:
    def __init__(self, installation, output, *, trusted=False, configuration_author=True):
        self.installation = installation.resolve()
        self.trusted = trusted
        self.configuration_author = configuration_author
        self.output = output
        self.temporary = tempfile.TemporaryDirectory(prefix="eden-grok-workflows-")
        self.root = Path(self.temporary.name)
        self.provider = probe.streaming.Provider()
        self.provider.allow.set()
        threading.Thread(target=self.provider.serve_forever, daemon=True).start()
        self.oauth = OAuthServer()
        self.device = DeviceServer()
        self.host = None
        self.endpoint = None
        self.terminal: probe.Terminal | None = None
        self.rpc: Rpc | None = None
        self.children = []
        self.checks = {}

    def __enter__(self):
        try:
            return self.start()
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def start(self):
        config = json.loads((self.installation / "composition.json").read_text())
        for package in config["packages"]:
            package["library"] = str((self.installation / package["library"]).resolve())
            if package["descriptor"]["package"] == "model-access":
                package["config"] = {
                    "catalog": {
                        "offline": True,
                        "models": [
                            {
                                "provider": provider,
                                "model": "workflow",
                                "api": "openai-completions",
                                "base_url": f"http://127.0.0.1:{self.provider.server_port}/v1",
                                "limits": {"context_window": 65536, "max_output_tokens": 4096},
                                "capabilities": {"tools": True, "images": False, "reasoning": True},
                                "compat": {"thinkingLevelMap": {"minimal": None, "max": "max"}},
                                "source": {"kind": "author", "location": "S4 workflow fixture"},
                            }
                            for provider in ["fixture", "anthropic", "kimi-coding"]
                        ],
                    },
                    "credentials": {
                        "path": str(self.root / "global/credentials.json"),
                        "oauth": {
                            "kimi-coding": {
                                "client_id": "fixture-device-client",
                                "device_url": self.device.base + "/device",
                                "token_url": self.device.base + "/token",
                            },
                            "anthropic": {
                                "client_id": "admitted-fixture-client",
                                "authorization_url": self.oauth.base + "/authorize",
                                "token_url": self.oauth.base + "/token",
                                "redirect_uri": "http://127.0.0.1:0/callback",
                            },
                        },
                    },
                }
        author = (
            ROOT
            / "target/verification/seeds/authors/configuration-forms/libauthor_configuration_forms.so"
        )
        if self.configuration_author and author.exists():
            config["packages"].append(
                {
                    "descriptor": {
                        "package": "configuration-forms",
                        "version": "0.1.0",
                        "provides": [
                            "eden.configuration.v1",
                            "eden.configuration.presentation.v1",
                            "author.configuration-forms.state.v1",
                        ],
                    },
                    "host": config["packages"][0]["host"],
                    "sdk": config["packages"][0]["sdk"],
                    "target": config["packages"][0]["target"],
                    "library": str(author),
                    "config": {
                        "label": "first",
                        "enabled": True,
                        "count": 2,
                        "ratio": 0.5,
                        "mode": "fast",
                        "nested": {"path": "./notes"},
                        "paths": ["one"],
                        "extra": {"preserved": True},
                        "restart_tag": "initial",
                    },
                }
            )
            config["roles"]["author.configuration-forms.state.v1"] = "configuration-forms"
        self.composition = self.root / "composition.json"
        self.composition.write_text(json.dumps(config))
        self.endpoint_file = self.root / "endpoint.json"
        self.history = self.root / "history.jsonl"
        self.host = subprocess.Popen(
            [
                str(self.installation / "bin/eden"),
                "--composition",
                str(self.composition),
                "--cwd",
                str(self.root),
                "--global-dir",
                str(self.root / "global"),
                "--session",
                str(self.history),
                "--offline-startup",
                "--trust-project" if self.trusted else "--no-trust-project",
                "live",
                "--endpoint",
                str(self.endpoint_file),
            ],
            cwd=self.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={**os.environ, "EDEN_TUI_STATE_DIR": str(self.root / "host-state")},
        )
        self.endpoint = probe.streaming.live.ready(self.host, self.endpoint_file)
        challenge = self.mutate(
            "/auth/start", {"request": {"action": "start", "provider": "fixture"}}
        )
        reply = self.call(
            "/auth/input",
            {
                "operation_id": challenge["operation_id"],
                "api_key": True,
                "input": "fixture-initial",
            },
        )
        self.call("/terminal", reply)
        self.mutate(
            "/models/select",
            {"selection": {"provider": "fixture", "model": "workflow", "thinking": "off"}},
        )
        self.rpc = Rpc(self.endpoint_file)
        self.identity = f"eden-{self.endpoint['session_id']}"
        self.rpc.call("session/load", {"sessionId": self.identity})
        return self

    def call(self, route, body=None):
        return probe.streaming.live.call(self.endpoint, route, body)

    def mutate(self, route, body):
        reply = self.call(route, {**body, "request_id": f"probe-{time.monotonic_ns()}"})
        result = self.call("/terminal", {"run_id": reply["run_id"]})
        assert result["outcome"]["status"] == "completed", result
        return result["outcome"]["value"]

    def ui(self, **params):
        assert self.rpc is not None
        return self.rpc.call("_eden/ui", {"sessionId": self.identity, **params})

    def form(self, name):
        return self.ui(open=name, formId=f"form-{time.monotonic_ns()}")

    def action(self, page, action, **values):
        return self.ui(formId=page["formId"], revision=page["revision"], action=action, **values)

    def capture(self, name):
        assert self.terminal is not None
        self.terminal.read(0.2)
        ansi = self.output / f"{name}.ansi"
        ansi.write_bytes(self.terminal.output)
        decoder = ROOT / "artifacts/g1-grok-port/eden-decode"
        if decoder.exists():
            screen = json.loads(
                subprocess.check_output(
                    [str(decoder), str(ansi), str(self.terminal.width), str(self.terminal.height)]
                )
            )
            (self.output / f"{name}.json").write_text(
                json.dumps(screen, ensure_ascii=False, indent=2)
            )
            (self.output / f"{name}.txt").write_text("\n".join(screen["screen"]["lines"]))

    def terminal_start(self, *, headless_browser=False):
        self.terminal = probe.Terminal(
            HERE / "run.py",
            self.endpoint_file,
            width=110,
            height=40,
            env={"DISPLAY": "", "WAYLAND_DISPLAY": "", "BROWSER": ""} if headless_browser else None,
        )
        self.terminal.wait("workflow", seconds=30)

    def click(self, label):
        assert self.terminal is not None
        self.terminal.wait(label, seconds=10)
        deadline = time.monotonic() + 15
        while "Working…" in self.terminal.display or "Loading…" in self.terminal.display:
            assert time.monotonic() < deadline, "form did not settle before the next action"
            self.terminal.read(0.05)
        lines = self.terminal.display.splitlines()
        row = next(index for index, line in enumerate(lines) if label in line)
        column = lines[row].index(label) + 2
        self.terminal.send(f"\x1b[<0;{column};{row + 1}M\x1b[<0;{column};{row + 1}m".encode())
        self.terminal.read(0.2)

    def stop_terminal(self):
        if not self.terminal or self.terminal.closed:
            return
        readers = []
        for path in (Path(self.terminal.state.name) / "hosts").glob("*.json"):
            endpoint = json.loads(path.read_text())
            try:
                if probe.streaming.live.call(endpoint, "/tui/snapshot")["state"]["read_only"]:
                    readers.append(endpoint)
                    self.children.append(endpoint)
            except (OSError, RuntimeError, KeyError):
                pass
        self.terminal.send(b"\x1b")
        self.terminal.send(b"\x15")
        self.terminal.command("/exit")
        until = time.monotonic() + 10
        while self.terminal.process.poll() is None and time.monotonic() < until:
            self.terminal.read(0.1)
        deadline = time.monotonic() + 5
        active = readers
        while active:
            remaining = []
            for endpoint in active:
                try:
                    probe.streaming.live.call(endpoint, "/tui/snapshot")
                    remaining.append(endpoint)
                except (OSError, RuntimeError):
                    pass
            assert not remaining or time.monotonic() < deadline, (
                "owned read-only host still serves after frontend exit"
            )
            active = remaining
            if active:
                time.sleep(0.05)
        if readers:
            self.checks["frontend_shutdown_closes_owned_readers"] = True
        self.terminal.close(screen=False)

    def __exit__(self, exc_type, exc, traceback):
        cleanup_error = None
        if exc:
            diagnostics = []
            for log in (self.root / "launcher").rglob("host.log"):
                content = log.read_text(errors="replace")
                for secret in (KEY, CONFIG_KEY, DEVICE_SECRET, DEVICE_ACCESS, *SECRETS):
                    content = content.replace(secret, "<REDACTED-FIXTURE-SECRET>")
                diagnostics.append(content)
            if diagnostics:
                (self.output / "failure-host.log").write_text("\n".join(diagnostics))
        if self.terminal and not self.terminal.closed:
            try:
                if exc:
                    self.capture("failure")
                self.stop_terminal()
            except (AssertionError, OSError, subprocess.TimeoutExpired) as error:
                cleanup_error = error
        if self.rpc:
            self.rpc.close()
        endpoint_files = list((self.root / "host-state").rglob("*.json")) + list(
            (self.root / "launcher").rglob("endpoint.json")
        )
        for endpoint_file in endpoint_files:
            try:
                endpoint = json.loads(endpoint_file.read_text())
                if (
                    isinstance(endpoint, dict)
                    and {"address", "token", "session_id"} <= endpoint.keys()
                ):
                    self.children.append(endpoint)
            except (ValueError, OSError):
                pass
        for endpoint in self.children:
            try:
                probe.streaming.live.call(endpoint, "/shutdown", {})
            except (OSError, RuntimeError):
                pass
        if self.endpoint:
            try:
                self.call("/shutdown", {})
            except (OSError, RuntimeError):
                pass
        if self.host:
            self.host.wait(timeout=15)
        self.oauth.close()
        self.device.close()
        self.provider.shutdown()
        self.provider.server_close()
        (self.output / "checks.json").write_text(json.dumps(self.checks, indent=2))
        self.temporary.cleanup()
        if exc_type is None and cleanup_error is not None:
            raise cleanup_error


class Rpc:
    def __init__(self, endpoint):
        self.process = subprocess.Popen(
            [str(ROOT / "artifacts/g1-native-port/eden-grok-adapter")],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env={**os.environ, "EDEN_GROK_ENDPOINT": str(endpoint)},
        )
        self.messages = queue.Queue()
        self.events = []
        self.sequence = 0

        output = self.process.stdout
        assert output is not None

        def read():
            for line in output:
                self.messages.put(json.loads(line))

        threading.Thread(target=read, daemon=True).start()

    def call(self, method, params, timeout=30):
        assert self.process.stdin is not None
        self.sequence += 1
        identity = self.sequence
        self.process.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": identity, "method": method, "params": params})
            + "\n"
        )
        self.process.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            reply = self.messages.get(timeout=max(0.1, deadline - time.monotonic()))
            if reply.get("id") == identity:
                if "error" in reply:
                    raise RuntimeError(reply["error"]["message"])
                return reply["result"]
            self.events.append(reply)
        raise AssertionError("adapter response deadline")

    def close(self):
        assert self.process.stdin is not None
        self.process.stdin.close()
        self.process.wait(timeout=15)


def rename(fixture):
    fixture.mutate("/manage/metadata", {"name": "Original", "tags": ["keep-tag"]})
    fixture.call(
        "/manage/rename",
        {
            "request_id": "saved-live-rename",
            "path": str(fixture.history),
            "name": "Live owner renamed",
            "preserve_tags": True,
            "tags": [],
        },
    )
    records = fixture.call("/tui/snapshot")["history"]
    metadata = next(
        record["payload"] for record in reversed(records) if record["kind"] == "session_metadata"
    )
    assert metadata == {"name": "Live owner renamed", "tags": ["keep-tag"]}, metadata
    fixture.checks["saved_rename_routes_to_live_owner_preserves_tags"] = True
    fixture.terminal_start()
    fixture.terminal.command("/rename PTY renamed")
    probe.streaming.live.wait_for(
        fixture.endpoint,
        lambda frame: any(
            record["kind"] == "session_metadata" and record["payload"]["name"] == "PTY renamed"
            for record in frame["history"]
        ),
    )
    fixture.capture("renamed")
    fixture.checks["rename_from_grok_command"] = True


def authentication(fixture):
    fixture.terminal_start(headless_browser=True)
    fixture.terminal.command("/auth")
    fixture.click("[ fixture ]")
    fixture.click("[ API key ]")
    fixture.terminal.wait("API key: <private input>")
    fixture.click("Submit private input")
    fixture.terminal.wait("Authentication failed")
    assert (
        json.loads((fixture.root / "global/credentials.json").read_text())["keys"]["fixture"]
        == "fixture-initial"
    )
    fixture.click("API key: <private input>")
    fixture.checks["private_auth_failure_retains_credentials_and_retry"] = True
    fixture.terminal.send(b"\x1b[200~" + KEY.encode() + b"\x1b[201~")
    fixture.capture("api-key-private")
    assert KEY not in fixture.terminal.output.decode(errors="replace")
    fixture.click("Submit private input")
    fixture.terminal.wait("Authentication status: completed")
    stored = json.loads((fixture.root / "global/credentials.json").read_text())
    assert stored["keys"]["fixture"] == KEY
    fixture.terminal.send(b"\x1b")
    fixture.checks["api_key_private_pty"] = True
    fixture.terminal.command("/auth")
    fixture.click("[ kimi-coding ]")
    fixture.click("[ Device login ]")
    fixture.terminal.wait("DEVICE-USER-CODE")
    fixture.capture("oauth-device")
    fixture.click("Open authorization page")
    fixture.terminal.wait("Browser unavailable; use Copy authorization URL")
    assert "Copy authorization URL" in fixture.terminal.display
    fixture.capture("oauth-browser-fallback")
    fixture.checks["oauth_browser_failure_retains_copy_url_exit"] = True
    fixture.device.allow.set()
    until = time.monotonic() + 15
    while "Authentication status: completed" not in fixture.terminal.display:
        assert time.monotonic() < until
        fixture.click("Check status")
        fixture.terminal.read(0.2)
    assert fixture.device.polls > 0
    fixture.terminal.send(b"\x1b")
    fixture.checks["oauth_device_polling_private_pty"] = True

    page = fixture.form("auth")
    page = fixture.action(page, "provider:anthropic")
    assert any(action["id"] == "browser" for action in page["actions"]), page
    page = fixture.action(page, "browser")
    page = fixture.action(page, "status")
    assert any(field["control"] == "secret" for field in page["fields"])
    url = page["description"].split("Open: ", 1)[1].splitlines()[0]
    parsed = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
    fixture.oauth.challenge = parsed["code_challenge"][0]
    redirect = (
        parsed["redirect_uri"][0]
        + "?"
        + urllib.parse.urlencode({"code": CODE, "state": parsed["state"][0]})
    )
    page = fixture.action(page, "submit", inputs={"input": redirect})
    until = time.monotonic() + 15
    while "completed" not in page["description"]:
        assert time.monotonic() < until, page["description"]
        page = fixture.action(page, "status")
        time.sleep(0.05)
    assert fixture.oauth.grants == ["authorization_code"]
    fixture.checks["oauth_private_code_exchange"] = True
    page = fixture.form("auth")
    page = fixture.action(page, "provider:anthropic")
    page = fixture.action(page, "browser")
    operation = page["authOperation"]
    fixture.action(page, "close")
    assert fixture.call("/auth/status", {"operation_id": operation})["status"] == "cancelled"
    assert fixture.oauth.grants == ["authorization_code"]
    fixture.checks["oauth_cancel_does_not_exchange"] = True
    assert not fixture.provider.requests
    public = fixture.history.read_text() + json.dumps(fixture.call("/tui/snapshot"))
    assert not any(
        secret in public for secret in (KEY, CONFIG_KEY, DEVICE_SECRET, DEVICE_ACCESS, *SECRETS)
    )
    for path in (fixture.root / ".grok-port").rglob("*"):
        if path.is_file():
            assert not any(
                secret.encode() in path.read_bytes()
                for secret in (KEY, CONFIG_KEY, DEVICE_SECRET, DEVICE_ACCESS, *SECRETS)
            ), str(path)
    fixture.checks["secrets_absent_from_prompt_history_and_logs"] = True


def configured(fixture):
    inspection = fixture.call("/configuration/inspect", {})
    return next(item for item in inspection["instances"] if item["id"] == "configuration-forms")


def configuration(fixture):
    page = fixture.form("config")
    page = fixture.action(page, "instance:configuration-forms")
    controls = {field["control"] for field in page["fields"]}
    assert {
        "text",
        "boolean",
        "integer",
        "number",
        "choice",
        "list",
        "json",
        "secret",
    } <= controls, controls
    page = fixture.action(
        page, "validate", edits=[{"operation": "set", "path": "/count", "value": 0}]
    )
    assert "minimum" in page["description"], page["description"]
    assert configured(fixture)["effective"]["count"] == 2
    page = fixture.action(page, "apply", edits=[{"operation": "set", "path": "/count", "value": 3}])
    assert configured(fixture)["effective"]["count"] == 3
    fixture.action(page, "close")
    fixture.checks["configuration_schema_validation_and_host_transaction"] = True
    fixture.terminal_start()
    fixture.terminal.command("/config")
    fixture.click("configuration-forms ·")
    fixture.terminal.wait("Count: 3")
    fixture.click("Count: 3")
    fixture.terminal.send(b"\x15" + b"4")
    fixture.click("Preview application")
    fixture.terminal.wait("Application: live")
    fixture.capture("configuration-preview")
    fixture.click("Apply to this Session")
    fixture.terminal.wait("Count: 4")
    assert configured(fixture)["effective"]["count"] == 4
    fixture.click("Enabled:")
    fixture.terminal.send(b" ")
    fixture.click("Mode:")
    fixture.terminal.send(b"\x1b[C")
    fixture.click("Paths:")
    fixture.terminal.send(b'\x15["one","two"]')
    fixture.click("Apply to this Session")
    until = time.monotonic() + 10
    while configured(fixture)["effective"].get("paths") != ["one", "two"]:
        assert time.monotonic() < until
        fixture.terminal.read(0.1)
    current = configured(fixture)["effective"]
    assert current["enabled"] is False and current["mode"] == "careful", current
    assert current["extra"] == {"preserved": True}
    fixture.checks["configuration_typed_controls_and_untouched_fields"] = True
    fixture.click("Private token:")
    fixture.terminal.send(b"\x1b[200~" + CONFIG_KEY.encode() + b"\x1b[201~")
    fixture.terminal.wait("Private token: •")
    fixture.capture("configuration-private")
    assert CONFIG_KEY not in fixture.terminal.output.decode(errors="replace")
    fixture.click("Preview application")
    fixture.terminal.wait("Application: restart")
    fixture.click("Apply to this Session")
    fixture.terminal.wait("<configured; unchanged>", seconds=15)
    assert configured(fixture)["secrets_configured"]["/token"] is True
    fixture.checks["configuration_private_restart_and_presence"] = True
    fixture.click("Count:")
    fixture.terminal.send(b"\x12")
    fixture.click("Apply to this Session")
    fixture.terminal.wait("Count: 2")
    assert configured(fixture)["effective"]["count"] == 2
    fixture.checks["configuration_inherit_preserves_scope"] = True
    fixture.terminal.resize(64, 24)
    fixture.capture("configuration-narrow")
    fixture.terminal.resize(110, 40)
    fixture.terminal.send(b"\x1b")
    assert CONFIG_KEY not in fixture.history.read_text()
    assert not fixture.provider.requests
    fixture.checks["configuration_does_not_send_prompt_or_secret_history"] = True


def history_recovery(fixture):
    old = json.loads(fixture.composition.read_text())
    old["packages"] = [
        item for item in old["packages"] if item["descriptor"]["package"] != "configuration-forms"
    ]
    old["roles"].pop("author.configuration-forms.state.v1", None)
    old_composition = fixture.root / "old-composition.json"
    old_composition.write_text(json.dumps(old))
    source = fixture.root / "older-history.jsonl"
    command = [
        str(fixture.installation / "bin/eden"),
        "--composition",
        str(old_composition),
        "--cwd",
        str(fixture.root),
        "--global-dir",
        str(fixture.root / "global"),
        "--session",
        str(source),
        "--offline-startup",
        "--no-trust-project",
        "models",
        "select",
        "fixture",
        "workflow",
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    assert result.returncode == 0, result.stderr
    original = source.read_bytes()
    try:
        fixture.call(
            "/manage/open", {"request_id": "mismatch-open", "path": str(source), "read_only": False}
        )
    except RuntimeError as error:
        assert "configuration-forms" in str(error), str(error)
    else:
        raise AssertionError("mismatched history was silently rebound")
    fixture.checks["binding_mismatch_names_changed_package"] = True
    rows = fixture.rpc.call("_x.ai/session/list", {})["sessions"]
    identity = next(row["sessionId"] for row in rows if row["sessionId"] == f"history:{source}")
    loaded = fixture.rpc.call("session/load", {"sessionId": identity})
    assert loaded["sessionId"] == identity
    try:
        fixture.rpc.call(
            "session/prompt",
            {
                "sessionId": identity,
                "prompt": [{"type": "text", "text": "must not execute"}],
                "_meta": {"promptId": "read-only"},
            },
        )
    except RuntimeError as error:
        assert "read-only" in str(error)
    else:
        raise AssertionError("read-only history accepted a prompt")
    assert source.read_bytes() == original
    assert not fixture.provider.requests
    fixture.checks["mismatch_falls_back_readonly_without_writes"] = True
    fixture.terminal_start()
    fixture.terminal.command("/sessions")
    fixture.click("older-history.jsonl")
    fixture.click("Read-only")
    fixture.terminal.wait("Read-only history.", seconds=20)
    fixture.capture("history-readonly")
    fixture.terminal.command("/sessions")
    fixture.click("older-history.jsonl")
    fixture.click("Preview migrated copy")
    fixture.terminal.wait("Review migrated copy", seconds=20)
    fixture.capture("history-migration-preview")
    destination = Path(str(source) + ".migrated.jsonl")
    fixture.terminal.send(b"\x1b")
    assert not destination.exists() and source.read_bytes() == original
    fixture.checks["cancelled_migration_does_not_write"] = True
    fixture.terminal.command("/sessions")
    fixture.click("older-history.jsonl")
    fixture.click("Preview migrated copy")
    fixture.terminal.wait("Review migrated copy", seconds=20)
    fixture.click("Create this copy and open it")
    fixture.terminal.wait("Session opened", seconds=20)
    until = time.monotonic() + 15
    while not destination.exists():
        assert time.monotonic() < until
        fixture.terminal.read(0.1)
    fixture.terminal.command("/rename Migrated copy")
    # A real mutation after load proves subsequent actions reached the new host.
    until = time.monotonic() + 15
    while b"Migrated copy" not in destination.read_bytes():
        assert time.monotonic() < until, fixture.terminal.display
        fixture.terminal.read(0.1)
    assert source.read_bytes() == original
    fixture.capture("history-migrated")
    fixture.checks["explicit_migration_creates_usable_copy_preserves_original"] = True
    fixture.stop_terminal()
    launch_source = fixture.history
    launch_original = launch_source.read_bytes()
    launcher = fixture.root / "launch-probe.py"
    launcher.write_text(
        "#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\n"
        + f"sys.path.insert(0, {str(HERE)!r})\nimport launch\n"
        + f"launch.ROOT = Path({str(fixture.root / 'launcher')!r})\n"
        + f"sys.argv = ['launch.py', '--host', {str(fixture.installation / 'bin/eden')!r}, '--cwd', {str(fixture.root)!r}, '--resume', {str(launch_source)!r}]\nlaunch.main()\n"
    )
    launcher.chmod(0o700)
    fixture.terminal = probe.Terminal(
        launcher,
        fixture.endpoint_file,
        width=110,
        height=40,
        env={"EDEN_AGENT_DIR": str(fixture.root / "global")},
    )
    fixture.terminal.wait("Read-only history.", seconds=35)
    fixture.terminal.wait("Session opened")
    fixture.capture("launcher-readonly")
    fixture.terminal.command("/sessions")
    fixture.click(str(launch_source))
    fixture.click("New copy path:")
    fixture.terminal.send(b"\x15" + str(fixture.root / "launcher-copy.jsonl").encode())
    fixture.click("Preview migrated copy")
    fixture.terminal.wait("Review migrated copy", seconds=20)
    fixture.terminal.send(b"\x1b")
    assert launch_source.read_bytes() == launch_original
    fixture.checks["launcher_resume_mismatch_has_readonly_and_migration_exit"] = True


def thinking_wire(fixture):
    fixture.provider.mode = "short"
    observed = []
    for requested, effective, sent, wire in [
        ("max", "max", "max", "max"),
        ("off", "off", "none", None),
        ("minimal", "low", "minimal", "low"),
    ]:
        fixture.rpc.call(
            "session/set_model",
            {
                "sessionId": fixture.identity,
                "modelId": "fixture/workflow",
                "_meta": {"reasoningEffort": sent},
            },
        )
        result = fixture.rpc.call(
            "session/prompt",
            {
                "sessionId": fixture.identity,
                "prompt": [{"type": "text", "text": f"Wire effort {requested}"}],
                "_meta": {"promptId": f"wire-{requested}"},
            },
        )
        assert result["stopReason"] == "end_turn"
        actual = fixture.provider.requests[-1].get("reasoning_effort")
        assert actual == wire, (requested, actual)
        if wire is None:
            assert "reasoning_effort" not in fixture.provider.requests[-1]
        assert fixture.call("/models/current", {})["effective_target"]["thinking"] == {
            "requested": requested,
            "effective": effective,
        }
        observed.append(
            {"requested": requested, "effective": effective, "wire_reasoning_effort": actual}
        )
    fixture.checks["thinking_final_provider_requests"] = observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/g1-native-host")
    parser.add_argument(
        "--case",
        choices=["rename", "auth", "config", "history", "recovery", "thinking", "all"],
        default="all",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output) as fixture:
        if args.case == "thinking":
            thinking_wire(fixture)
        if args.case in {"rename", "all"}:
            rename(fixture)
        if args.case in {"auth", "all"}:
            if fixture.terminal:
                fixture.stop_terminal()
                fixture.terminal = None
            authentication(fixture)
        for case, run in [("config", configuration), ("history", history_recovery)]:
            if args.case in {case, "recovery", "all"}:
                if fixture.terminal:
                    fixture.stop_terminal()
                    fixture.terminal = None
                run(fixture)


if __name__ == "__main__":
    main()
