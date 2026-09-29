#!/usr/bin/env python3
"""Exercise native-author configuration forms in the installed TUI and real Chromium."""

import argparse
import importlib.util
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

from install import ROOT, build_target, library, package, target
from verification import author_artifact, installed, prepare

HELPER = "configuration-forms"
CUSTOM = "configuration-custom"
DEFAULT_MODEL = "model-access"
CONFIGURATION = "eden.configuration.v1"
FORM = "eden.configuration.presentation.v1"

# Reuse the installed live-host transport and composition vocabulary from its acceptance suite.
spec = importlib.util.spec_from_file_location(
    "presentation_acceptance", ROOT / "scripts/verify-presentation.py"
)
assert spec and spec.loader
presentation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(presentation)
call = presentation.call
wait_for = presentation.wait_for


def configuration(endpoint: dict, instance: str) -> dict:
    inspection = call(endpoint, "/configuration/inspect", {})
    return next(item for item in inspection["instances"] if item["id"] == instance)


def nodes(items: list[dict]):
    for item in items:
        yield item
        yield from nodes(item.get("children", []))


def form(endpoint: dict, instance: str) -> tuple[dict, dict]:
    snapshot = call(endpoint, "/snapshot")["presentation"]
    for view in snapshot["views"]:
        for node in nodes(view["nodes"]):
            if node["kind"] == "configuration_form" and node["binding"]["instance"] == instance:
                assert view["scope"] == "management" and view["run_id"] == 0
                return view, node
    raise AssertionError(f"No configuration form for {instance}")


def await_value(endpoint: dict, instance: str, key: str, expected: Any) -> dict:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        item = configuration(endpoint, instance)
        if item["effective"].get(key) == expected:
            return item
        time.sleep(0.1)
    raise AssertionError((instance, key, expected, configuration(endpoint, instance)))


class Browser:
    """Own one isolated browser session and collect actual rendered screenshots."""

    def __init__(self, endpoint: dict, output: pathlib.Path):
        self.session = f"configuration-native-{os.getpid()}"
        self.output = output
        self.endpoint = endpoint
        self.run("open", f"http://{endpoint['address']}/?token={endpoint['token']}")
        self.wait('document.body.innerText.includes("Connected to the shared session")')
        # Capture only operation identity and outcome, never request values or secret material.
        self.evaluate("""(() => {
            const original = window.fetch;
            window.configurationProbe = null;
            window.fetch = async (...args) => {
                const path = new URL(String(args[0]), location.href).pathname;
                const probe = window.configurationProbe;
                let matches = false;
                if (probe && ['/action', '/private-input', '/configuration/open'].includes(path)) {
                    const body = JSON.parse(args[1]?.body ?? '{}');
                    const request = path === '/private-input' ? body.request : body;
                    matches = path === '/configuration/open'
                        ? probe.action === 'open' && request.instance === probe.instance
                        : request.values?.binding?.instance === probe.instance
                          && request.action?.split(':').pop() === probe.action;
                }
                try {
                    const response = await original(...args);
                    if (matches) {
                        const reply = await response.clone().json();
                        probe.result = {
                            ok: reply.ok,
                            code: reply.error?.code,
                            status: reply.result?.status,
                            application: reply.result?.application,
                            validation: Array.isArray(reply.result?.errors),
                        };
                    }
                    return response;
                } catch (error) {
                    if (matches) probe.result = {ok: false, code: 'TransportFailure'};
                    throw error;
                }
            };
        })()""")

    def run(self, *args: str) -> Any:
        result = subprocess.run(
            ["agent-browser", "--session", self.session, "--json", *args],
            capture_output=True,
            text=True,
            check=False,
            timeout=60 if args and args[0] == "open" else 25,
        )
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        value = json.loads(result.stdout)
        assert value["success"], value
        return value["data"]

    def evaluate(self, expression: str) -> Any:
        return self.run("eval", expression)["result"]

    def wait(self, expression: str) -> None:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.evaluate(expression):
                return
            time.sleep(0.1)
        raise AssertionError((expression, self.evaluate("document.body.innerText")))

    def fill(self, label: str, value: str) -> None:
        self.run("find", "label", label, "fill", value, "--exact")

    def begin_operation(self, instance: str, action: str) -> None:
        self.evaluate(
            f"window.configurationProbe = {{instance: {json.dumps(instance)}, action: {json.dumps(action)}, result: null}}"
        )

    def await_operation(self, instance: str, action: str) -> dict:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            result = self.evaluate("window.configurationProbe.result")
            if result is not None:
                assert result.get("ok"), (instance, action, result)
                return result
            time.sleep(0.1)
        raise AssertionError((instance, action, "matching operation did not complete"))

    def current_binding(self, instance: str) -> None:
        # Server completion precedes the polling adapter's React render.
        _, node = form(self.endpoint, instance)
        binding = node["binding"]
        label = (
            f"Instance {instance} · configuration revision {binding['revision']} · "
            f"generation {binding['generation'] if binding['generation'] is not None else 'not running'}"
        )
        self.wait(
            f"Array.from(document.querySelectorAll('form.configuration > p')).some(node => node.textContent === {json.dumps(label)})"
        )

    def open_settings(self, instance: str) -> None:
        self.fill("Configuration instance", instance)
        self.begin_operation(instance, "open")
        self.run("find", "role", "button", "click", "--name", "Open settings", "--exact")
        self.await_operation(instance, "open")
        self.current_binding(instance)

    def action(self, instance: str, action: str, *, expected_status: str = "applied") -> None:
        server_action = action in {"validate", "preview", "apply", "refresh", "cancel_apply"}
        if server_action:
            self.begin_operation(instance, action)
        clicked = self.evaluate(
            f"(() => {{ const form = Array.from(document.querySelectorAll('form.configuration')).find(form => form.innerText.includes('Instance {instance} ·')); const button = Array.from(form.querySelectorAll('button')).find(button => button.textContent === {json.dumps(action)}); if (button.disabled) return false; button.click(); return true; }})()"
        )
        if server_action:
            assert clicked, (instance, action, "server action is disabled")
            result = self.await_operation(instance, action)
            if action in {"apply", "cancel_apply"}:
                assert result.get("status") == expected_status, (instance, action, result)
            elif action == "validate":
                assert result.get("validation"), (instance, action, result)
            elif action == "preview":
                assert result.get("application") or result.get("validation"), (
                    instance,
                    action,
                    result,
                )
        self.wait('!document.body.innerText.includes("Configuration request pending…")')
        if server_action and action in {"apply", "refresh", "cancel_apply"}:
            self.current_binding(instance)

    def fill_field(self, instance: str, path: str, value: str) -> None:
        self.current_binding(instance)
        self.evaluate(
            f"Array.from(document.querySelectorAll('form.configuration')).find(form => form.innerText.includes('Instance {instance} ·')).querySelectorAll('fieldset') && Array.from(Array.from(document.querySelectorAll('form.configuration')).find(form => form.innerText.includes('Instance {instance} ·')).querySelectorAll('fieldset')).find(field => field.querySelector('legend').textContent.includes('({path})')).querySelector('input').setAttribute('data-budget-probe', 'selected')"
        )
        self.run("fill", "input[data-budget-probe=selected]", value)
        self.evaluate(
            "document.querySelector('input[data-budget-probe=selected]').removeAttribute('data-budget-probe')"
        )

    def field_action(self, instance: str, path: str, action: str) -> None:
        self.evaluate(
            f"Array.from(Array.from(Array.from(document.querySelectorAll('form.configuration')).find(form => form.innerText.includes('Instance {instance} ·')).querySelectorAll('fieldset')).find(field => field.querySelector('legend').textContent.includes('({path})')).querySelectorAll('button')).find(button => button.textContent === {json.dumps(action)}).click()"
        )

    def screenshot(self, name: str) -> None:
        self.run("screenshot", str(self.output / f"{name}.png"), "--full")

    def close(self) -> None:
        self.run("close")


class TerminalTransport:
    """Stamp snapshot titles so a rendered palette proves which snapshot the TUI consumed."""

    def __init__(self, endpoint: dict, directory: pathlib.Path):
        self.receipts: list[dict] = []
        self.errors: list[str] = []
        self.closed = False
        self.snapshot_held = threading.Event()
        self.snapshots = threading.Event()
        self.snapshots.set()
        transport = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, format: str, *args):
                pass

            def do_GET(self):
                self.handle_forward()

            def do_POST(self):
                self.handle_forward()

            def handle_forward(self):
                try:
                    self.forward()
                except Exception as error:
                    transport.errors.append(type(error).__name__)
                    self.send_error(502, "acceptance transport failed")

            def forward(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                request = urllib.request.Request(
                    f"http://{endpoint['address']}{self.path}",
                    data=body if self.command == "POST" else None,
                    headers={k: v for k, v in self.headers.items() if k.lower() != "host"},
                    method=self.command,
                )
                try:
                    response = urllib.request.urlopen(request, timeout=35)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    payload, status = response.read(), response.getcode()
                assert isinstance(status, int)
                value = json.loads(payload)
                if self.path.startswith("/tui/snapshot") and value.get("ok"):
                    for view in value["result"]["presentation"]["views"]:
                        view["title"] = transport.title(view)
                    payload = json.dumps(value).encode()
                    if not transport.snapshots.is_set():
                        transport.snapshot_held.set()
                    assert transport.snapshots.wait(30), "snapshot release was not signalled"
                if self.path in ("/action", "/private-input"):
                    action = json.loads(body)
                    if self.path == "/private-input":
                        action = action["request"]
                    # Record identities and results only, never field values or private inputs.
                    transport.receipts.append(
                        {
                            "request_id": action["request_id"],
                            "action": action["action"],
                            "revision": action["revision"],
                            "binding": action["values"].get("binding"),
                            "result": value,
                        }
                    )
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except BrokenPipeError:
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = False
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.endpoint_file = directory / "terminal-endpoint.json"
        self.endpoint_file.write_text(
            json.dumps(
                {
                    **endpoint,
                    "address": f"127.0.0.1:{self.server.server_port}",
                }
            ),
            encoding="utf-8",
        )

    @staticmethod
    def title(view: dict) -> str:
        return f"{view['title']} [snapshot {view['revision']}]"

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.snapshots.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        assert not self.errors, self.errors


class Terminal:
    """Navigate official plugin dialogs while keeping D2 assertions on host facts."""

    def __init__(self, binary: pathlib.Path, endpoint_file: pathlib.Path, endpoint: dict):
        from tui_pty import Terminal as PtyTerminal

        self.endpoint = endpoint
        self.transport = TerminalTransport(endpoint, endpoint_file.parent)
        self.driver = PtyTerminal(binary, self.transport.endpoint_file, width=180, height=50)
        self.output = self.driver.output
        self.focus: tuple[str, str] | None = None
        self.instance: str | None = None
        self.selected = 0
        self.driver.wait("Connected")

    def drain(self, seconds: float = 0.35) -> None:
        self.driver.read(seconds)
        assert self.driver.process.poll() is None, self.display

    def keys(self, value: bytes) -> None:
        self.driver.send(value)

    def field(self, instance: str, path: str) -> None:
        view, node = form(self.endpoint, instance)
        paths = [field["path"] for field in node["fields"]]
        if self.instance != instance:
            if self.instance is not None:
                self.keys(b"\x1b")
            self.driver.command("/live")
            self.driver.wait(TerminalTransport.title(view))
            entries = [
                (candidate["owner"], candidate["id"], item["id"])
                for candidate in call(self.endpoint, "/snapshot")["presentation"]["views"]
                for item in nodes(candidate["nodes"])
                if item["kind"] in ("configuration_form", "form", "button")
            ]
            index = entries.index((view["owner"], view["id"], node["id"]))
            self.keys(b"\x1b[B" * index + b"\r")
            self.driver.wait("Ctrl+S Apply")
            self.instance, self.selected = instance, 0
        selected = [
            index
            for index, field in enumerate(node["fields"])
            if re.search(r"› " + re.escape(field["label"]) + r"\s", self.display)
        ]
        assert len(selected) == 1, ("selected field is not visible", self.display)
        self.selected = selected[0]
        new = paths.index(path)
        self.keys(b"\t" * ((new - self.selected) % len(paths)))
        self.driver.wait("› " + node["fields"][new]["label"])
        self.selected = new
        self.focus = (instance, path)

    def edit(self, instance: str, path: str, value: str) -> None:
        self.field(instance, path)
        self.keys(b"\x15" + value.encode())

    @property
    def display(self) -> str:
        return self.driver.display

    def synchronize_snapshot(self) -> None:
        # Closing/reopening saves the ordinary draft with its original binding. A
        # stamped palette entry proves the newer snapshot is consumed before Refresh.
        assert self.instance is not None
        instance = self.instance
        path = (
            self.focus[1] if self.focus else form(self.endpoint, instance)[1]["fields"][0]["path"]
        )
        self.keys(b"\x1b")
        self.instance = None
        self.field(instance, path)

    def action(self, key: bytes, *, rejected: bool = False) -> None:
        if key == b"\x04":
            self.keys(key)
            self.selected = 0
            self.focus = None
            return
        if key == b"\x06":
            self.synchronize_snapshot()
        before = len(self.transport.receipts)
        self.keys(key)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            self.drain(0.1)
            receipts = self.transport.receipts[before:]
            if receipts:
                assert len(receipts) == 1, receipts
                receipt = receipts[0]
                result = receipt["result"]
                if not result["ok"]:
                    self.driver.wait(result["error"]["code"])
                    raise AssertionError(receipt)
                value = result["result"]
                if key == b"\x06":
                    if "Refreshed · draft retained" in self.display and any(
                        f'"value":{item["value"]}' in self.display for item in value
                    ):
                        return
                else:
                    marker = (
                        f'"operation":{value["operation"]},'
                        if isinstance(value, dict) and "operation" in value
                        else json.dumps(value, separators=(",", ":"), sort_keys=True)[:64]
                    )
                    if marker in self.display:
                        return
            elif rejected and "Configuration changed" in self.display:
                # A local CAS rejection has no host receipt.
                return
        raise AssertionError((self.instance, "PTY action receipt was not rendered", self.display))

    def applied(self, receipt: dict) -> None:
        # A committed configuration is not evidence that the frontend has cleared its
        # pending request. Observe this operation's receipt before sending another key.
        self.driver.wait(f'"operation":{receipt["operation"]}', seconds=20)

    def close(self, output: pathlib.Path) -> None:
        try:
            self.driver.close()
        finally:
            output.write_bytes(self.output)


def composition() -> dict:
    default = {
        "label": "first",
        "enabled": True,
        "count": 2,
        "ratio": 0.5,
        "mode": "fast",
        "nested": {"path": "./notes"},
        "paths": ["one"],
        "extra": {"preserved": True},
        "restart_tag": "initial",
    }
    custom = {
        "style": "direct",
        "endpoint": "https://example.invalid",
        "policy": {"retries": 2, "enabled": True},
        "tags": ["local"],
    }
    packages = [
        package(
            "presentation-live",
            presentation.PROVIDES,
            str(author_artifact("presentation-live")),
            target(),
        )
    ]
    roles = {role: "presentation-live" for role in presentation.PROVIDES}
    packages.extend(
        [
            package(
                "coding",
                [
                    "eden.coding-control.v1",
                    presentation.CODING,
                    "eden.context-edit.v1",
                    "eden.compaction-policy.v1",
                    presentation.CODING_CONTEXT,
                    presentation.QUEUE,
                ],
                str(build_target() / "debug" / library("eden_coding")),
                target(),
            ),
            package(
                "local-history",
                [presentation.STORE],
                str(build_target() / "debug" / library("eden_local_history")),
                target(),
            ),
        ]
    )
    roles[presentation.QUEUE] = "coding"
    roles[presentation.STORE] = "local-history"
    packages.extend(
        [
            package(
                "note-style-context-management",
                [
                    "eden.compaction-policy.v1",
                    "eden.record-interpreter.v1",
                    "eden.state-migrator.v1",
                    CONFIGURATION,
                    "eden.history-recall.v1",
                    "eden.history-recall-tools.v1",
                    "eden.history-recall-tool.v1",
                ],
                str(build_target() / "debug" / library("eden_note_style_context_management")),
                target(),
                {"max_output_tokens": 2048, "timeout_ms": 60000, "max_input_bytes": 262144},
            ),
            package(
                "cache-warmer",
                [
                    "eden.instance-ready.v1",
                    "eden.cache-warmer.watch.v1",
                    "eden.cache-warmer.work.v1",
                    "eden.cache-warmer.v1",
                    CONFIGURATION,
                ],
                str(build_target() / "debug" / library("eden_cache_warmer")),
                target(),
                {
                    "mode": "off",
                    "interval_ms": 60000,
                    "ttl_ms": 300000,
                    "safety_ms": 15000,
                    "timeout_ms": 10000,
                    "max_requests": 3,
                    "max_output_tokens": 1,
                },
            ),
        ]
    )
    # Mount the shipped implementation for targeted configuration calls without replacing fixture roles.
    packages.append(
        package(
            DEFAULT_MODEL,
            [
                "eden.model-catalog.v1",
                "eden.model-manager.v1",
                "eden.credential-source.v1",
                "eden.auth.v1",
                CONFIGURATION,
                "eden.auxiliary-model.v1",
                "eden.model-info.v1",
                "eden.coding-provider.v1",
            ],
            str(build_target() / "debug" / library("eden_model_access")),
            target(),
            {},
        )
    )
    roles["eden.record-interpreter.v1"] = "note-style-context-management"
    roles["eden.cache-warmer.v1"] = "cache-warmer"
    for name, config in [(HELPER, default), (CUSTOM, custom)]:
        state = f"author.{name}.state.v1"
        packages.append(
            package(
                name, [CONFIGURATION, FORM, state], str(author_artifact(name)), target(), config
            )
        )
        roles[state] = name
    return {"packages": packages, "roles": roles, "resource_packages": []}


def exercise(
    endpoint: dict, terminal: Terminal | None, browser: Browser, output: pathlib.Path
) -> dict:
    browser.open_settings(HELPER)
    if terminal:
        terminal.driver.command(f"/config {CUSTOM}")
    else:
        browser.open_settings(CUSTOM)
    browser.wait('document.body.innerText.includes("Direct author settings")')
    assert configuration(endpoint, HELPER)["generation"] is not None
    assert configuration(endpoint, CUSTOM)["generation"] is not None
    browser.fill("Label", "forbidden")
    browser.action(HELPER, "validate")
    browser.wait('document.querySelector("footer").innerText.includes("plugin_validation")')
    assert configuration(endpoint, HELPER)["effective"]["label"] == "first"
    browser.action(HELPER, "Discard draft and use current values")
    browser.fill("Count", "4")
    browser.action(HELPER, "validate")
    browser.wait('document.querySelector("footer").innerText.includes("errors")')
    browser.action(HELPER, "preview")
    browser.wait('document.querySelector("footer").innerText.includes("live")')
    browser.action(HELPER, "apply")
    await_value(endpoint, HELPER, "count", 4)
    browser.fill("Count", "5")
    browser.action(HELPER, "apply")
    await_value(endpoint, HELPER, "count", 5)
    # Two path-level list operations use the real adapter controls.
    browser.action(HELPER, "Add item")
    browser.fill("Item 2", "two")
    browser.action(HELPER, "Move item 2 up")
    browser.action(HELPER, "Delete item 2")
    browser.action(HELPER, "apply")
    await_value(endpoint, HELPER, "paths", ["two"])
    assert configuration(endpoint, HELPER)["effective"]["extra"] == {"preserved": True}
    browser.fill("JSON fallback (non-secret subtree)", "null")
    browser.action(HELPER, "apply")
    await_value(endpoint, HELPER, "extra", None)
    browser.screenshot("web-default-helper")
    # Refresh custom's independent view after the shared configuration revision advanced.
    browser.action(CUSTOM, "refresh")
    browser.fill("Destination", "https://web.invalid")
    browser.action(CUSTOM, "validate")
    browser.action(CUSTOM, "preview")
    browser.action(CUSTOM, "apply")
    await_value(endpoint, CUSTOM, "endpoint", "https://web.invalid")
    browser.screenshot("web-direct-author")
    # Restore an explicit override to the inherited configuration through its own semantic edit.
    browser.evaluate(
        "Array.from(document.querySelectorAll('fieldset')).find(field => field.querySelector('legend').textContent.includes('/endpoint')).querySelectorAll('button')[1].click()"
    )
    browser.action(CUSTOM, "apply")
    await_value(endpoint, CUSTOM, "endpoint", "https://example.invalid")
    if terminal:
        terminal.drain()
        terminal.field(HELPER, "/count")
        terminal.action(b"\x06")
        terminal.edit(HELPER, "/count", "6")
        terminal.action(b"\x16")  # validate
        terminal.action(b"\x10")  # preview
        terminal.action(b"\x13")  # apply
        await_value(endpoint, HELPER, "count", 6)
        terminal.drain()
        terminal.edit(HELPER, "/count", "7")
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 7)
        terminal.field(HELPER, "/paths")
        terminal.edit(HELPER, "/paths", '["three", "two"]')
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "paths", ["three", "two"])
        terminal.edit(HELPER, "/paths", '["two"]')
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "paths", ["two"])
        terminal.field(HELPER, "/count")
        terminal.keys(b"\x02")  # restore inheritance, not the schema default
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 2)
        terminal.keys(b"\x05")  # clear this field, distinct from writing null
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", None)
        assert "count" not in configuration(endpoint, HELPER)["effective"]
        terminal.edit(HELPER, "/count", "7")
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 7)
        terminal.field(CUSTOM, "/endpoint")
        terminal.action(b"\x06")
        terminal.edit(CUSTOM, "/endpoint", "forbidden")
        terminal.action(b"\x16")
        assert "plugin_validation" in terminal.display, terminal.display
        assert configuration(endpoint, CUSTOM)["effective"]["endpoint"] == "https://example.invalid"
        terminal.edit(CUSTOM, "/endpoint", "https://tui.invalid")
        terminal.action(b"\x16")
        terminal.action(b"\x10")
        terminal.action(b"\x13")
        await_value(endpoint, CUSTOM, "endpoint", "https://tui.invalid")
    if terminal:
        terminal.field(HELPER, "/label")
        terminal.action(b"\x06")
        terminal.edit(HELPER, "/label", "terminal stale draft")
        browser.action(HELPER, "refresh")
        browser.fill("Count", "8")
        browser.action(HELPER, "apply")
        await_value(endpoint, HELPER, "count", 8)
        terminal.synchronize_snapshot()
        revision = call(endpoint, "/configuration/inspect", {})["revision"]
        terminal.action(b"\x13", rejected=True)
        assert call(endpoint, "/configuration/inspect", {})["revision"] == revision
        assert "Configuration changed" in terminal.display, terminal.display
        terminal.action(b"\x04")
    # A pending web draft survives a whole chat run, then conflicts with the TUI apply.
    browser.action(HELPER, "refresh")
    browser.action(HELPER, "Discard draft and use current values")
    browser.fill("Label", "web retained draft")
    if terminal:
        terminal.edit(HELPER, "/count", "9")
    run = call(endpoint, "/prompt", {"request_id": "configuration-chat", "text": "show"})["run_id"]
    wait_for(
        endpoint,
        lambda frame: any(
            view["id"] == "review" and view["active"] for view in frame["presentation"]["views"]
        ),
    )
    call(endpoint, "/cancel", {"run_id": run})
    wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
    browser.wait(
        'Array.from(document.querySelectorAll("label")).find(label => label.textContent === "Label").querySelector("input").value === "web retained draft"'
    )
    if terminal:
        terminal.drain()
        assert re.search(r"Count\s+9", terminal.display), terminal.display
        terminal.action(b"\x16")
        terminal.action(b"\x10")
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 9)
        browser.wait('document.body.innerText.includes("Configuration changed")')
        browser.action(HELPER, "Discard draft and use current values")
        browser.fill("Label", "web retained draft after restart")
    peer = call(endpoint, "/attach", {"frontend": "web"})["attachment"]
    before = configuration(endpoint, HELPER)["generation"]
    if terminal:
        terminal.action(b"\x06")
        terminal.edit(HELPER, "/restart_tag", "restarted")
        terminal.action(b"\x06")
        terminal.action(b"\x04")
        terminal.edit(HELPER, "/restart_tag", "restarted")
        terminal.action(b"\x13")
    else:
        browser.action(HELPER, "Discard draft and use current values")
        browser.fill("Restart tag", "restarted")
        browser.action(HELPER, "apply")
    await_value(endpoint, HELPER, "restart_tag", "restarted")
    assert configuration(endpoint, HELPER)["generation"] != before
    assert str(call(endpoint, f"/snapshot?attachment={peer}")["presentation"]["session_id"]) == str(
        endpoint["session_id"]
    )
    if terminal:
        browser.wait('document.body.innerText.includes("Configuration changed")')
        assert browser.evaluate(
            "Array.from(document.querySelectorAll('form.configuration')).find(form => form.innerText.includes('Instance configuration-forms ·')).querySelector('input').matches(':disabled')"
        )
        browser.screenshot("web-conflict-after-native-restart")
    call(endpoint, "/detach", {"attachment": peer})
    # Switch the second independent author from direct construction to customized helper output.
    browser.action(CUSTOM, "refresh")
    browser.action(CUSTOM, "Discard draft and use current values")
    browser.run("select", "select:has(option[value='\"helper\"'])", '"helper"')
    browser.action(CUSTOM, "preview")
    browser.action(CUSTOM, "apply")
    await_value(endpoint, CUSTOM, "style", "helper")
    browser.wait('document.body.innerText.includes("Customized helper settings")')
    browser.fill("Destination (author label)", "https://customized.invalid")
    browser.action(CUSTOM, "validate")
    browser.action(CUSTOM, "preview")
    browser.action(CUSTOM, "apply")
    await_value(endpoint, CUSTOM, "endpoint", "https://customized.invalid")
    if terminal:
        terminal.drain()
        terminal.edit(CUSTOM, "/endpoint", "https://customized-tui.invalid")
        terminal.action(b"\x16")
        terminal.action(b"\x10")
        terminal.action(b"\x13")
        await_value(endpoint, CUSTOM, "endpoint", "https://customized-tui.invalid")
    browser.screenshot("web-customized-helper")
    for instance, budgets in [
        ("note-style-context-management", [256, 384, 512, 640]),
        ("cache-warmer", [2, 3, 4, 5]),
    ]:
        browser.open_settings(instance)
        for budget in budgets[:2]:
            browser.fill_field(instance, "/max_output_tokens", str(budget))
            browser.action(instance, "validate")
            browser.action(instance, "preview")
            browser.action(instance, "apply")
            await_value(endpoint, instance, "max_output_tokens", budget)
        if terminal:
            terminal.drain()
            for budget in budgets[2:]:
                terminal.edit(instance, "/max_output_tokens", str(budget))
                terminal.action(b"\x16")
                terminal.action(b"\x10")
                terminal.action(b"\x13")
                await_value(endpoint, instance, "max_output_tokens", budget)
        browser.screenshot(f"web-business-{instance}")
    assert configuration(endpoint, "cache-warmer")["effective"]["mode"] == "off"
    call(endpoint, "/configuration/open", {"instance": "coding"})
    _, coding_form = form(endpoint, "coding")
    if configuration(endpoint, "coding")["description"]["schema"] is None:
        assert any(field["control"] == "json" for field in coding_form["fields"])
    browser.wait("document.body.innerText.includes('Instance coding ·')")
    browser.screenshot("web-default-coding-fallback")
    inspection = call(endpoint, "/configuration/inspect", {})
    assert all(item["status"] == "applied" for item in inspection["operations"])
    return {
        "pty": terminal is not None,
        "browser": True,
        "inspection": inspection,
        "snapshot": call(endpoint, "/snapshot"),
    }


def refresh_draft_evidence(endpoint: dict, terminal: Terminal) -> None:
    """A newer consumed snapshot must let Refresh retain an ordinary stale draft."""
    call(endpoint, "/configuration/open", {"instance": HELPER})
    terminal.edit(HELPER, "/label", "retained refresh draft")
    original = configuration(endpoint, HELPER)["effective"]["label"]
    result = call(
        endpoint,
        "/action",
        action_request(
            endpoint,
            HELPER,
            "apply",
            [
                {"operation": "set", "path": "/label", "value": "external refresh marker"},
            ],
        ),
    )
    assert result["status"] == "applied", result
    terminal.synchronize_snapshot()
    terminal.action(b"\x13", rejected=True)
    assert "Configuration changed" in terminal.display, terminal.display
    before = call(endpoint, "/configuration/inspect", {})["revision"]
    terminal.action(b"\x06")
    assert "retained refresh draft" in terminal.display, terminal.display
    assert configuration(endpoint, HELPER)["effective"]["label"] == "external refresh marker"
    assert call(endpoint, "/configuration/inspect", {})["revision"] == before
    terminal.action(b"\x13")
    assert configuration(endpoint, HELPER)["effective"]["label"] == "retained refresh draft"
    # Restore the fixture through an ordinary acknowledged edit for the remaining cases.
    terminal.edit(HELPER, "/label", original)
    terminal.action(b"\x13")
    assert configuration(endpoint, HELPER)["effective"]["label"] == original
    terminal.keys(b"\x1b")
    terminal.instance = None
    terminal.focus = None


def headless(endpoint: dict, terminal: Terminal | None, reason: str) -> dict:
    """Keep native contracts and available PTY evidence active when CI lacks Chromium tooling."""
    for instance in (HELPER, CUSTOM):
        call(endpoint, "/configuration/open", {"instance": instance})
    receipts = []
    request_sequence = 0
    for instance, path, value in [
        (HELPER, "/count", 4),
        (HELPER, "/count", 5),
        (CUSTOM, "/endpoint", "https://headless.invalid"),
        (CUSTOM, "/style", "helper"),
        ("note-style-context-management", "/max_output_tokens", 256),
        ("note-style-context-management", "/max_output_tokens", 384),
        ("cache-warmer", "/max_output_tokens", 2),
        ("cache-warmer", "/max_output_tokens", 3),
    ]:
        call(endpoint, "/configuration/open", {"instance": instance})
        for action in ("validate", "preview", "apply"):
            view, node = form(endpoint, instance)
            request_sequence += 1
            result = call(
                endpoint,
                "/action",
                {
                    "session_id": endpoint["session_id"],
                    "owner": view["owner"],
                    "view_id": view["id"],
                    "revision": view["revision"],
                    "action": f"{node['id']}:{action}",
                    "request_id": f"native-{request_sequence}",
                    "values": {
                        "binding": node["binding"],
                        "edits": [{"operation": "set", "path": path, "value": value}],
                    },
                },
            )
            if action == "validate":
                assert result["errors"] == []
            if action == "apply":
                assert result["status"] == "applied"
                receipts.append(result)
    if terminal:
        terminal.drain()
        terminal.field(HELPER, "/count")
        terminal.action(b"\x06")
        terminal.edit(HELPER, "/count", "6")
        terminal.action(b"\x16")
        terminal.action(b"\x10")
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 6)
        terminal.edit(HELPER, "/count", "7")
        terminal.action(b"\x13")
        await_value(endpoint, HELPER, "count", 7)
    return {
        "pty": terminal is not None,
        "browser": False,
        "browser_skip_reason": reason,
        "receipts": receipts,
        "inspection": call(endpoint, "/configuration/inspect", {}),
    }


def assert_private_absent(value: Any) -> None:
    text = value if isinstance(value, str) else json.dumps(value)
    assert not re.search(r"D2_[A-Z_]*CANARY", text), "Private material escaped into public output"


def action_request(endpoint: dict, instance: str, action: str, edits: list[dict]) -> dict:
    view, node = form(endpoint, instance)
    return {
        "session_id": endpoint["session_id"],
        "owner": view["owner"],
        "view_id": view["id"],
        "revision": view["revision"],
        "action": f"{node['id']}:{action}",
        "request_id": f"private-native-{time.monotonic_ns()}",
        "values": {"binding": node["binding"], "edits": edits},
    }


def await_private_operation(
    endpoint: dict, revision: int, status: str, terminal: Terminal | None = None
) -> dict:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        inspection = call(endpoint, "/configuration/inspect", {})
        if inspection["revision"] > revision:
            receipt = max(inspection["operations"], key=lambda item: item["operation"])
            assert receipt["status"] == status, receipt
            if terminal:
                terminal.applied(receipt)
            return receipt
        if terminal:
            terminal.drain(0.1)
        else:
            time.sleep(0.1)
    raise AssertionError("Private configuration operation did not settle")


def private_evidence(endpoint: dict, terminal: Terminal | None, browser: Browser | None) -> dict:
    """PC-06: native material delivery, shared transactions, masking and material-free retry."""
    receipts = []
    unrelated = configuration(endpoint, "note-style-context-management")["generation"]

    def public_surfaces() -> None:
        assert_private_absent(call(endpoint, "/snapshot"))
        peer = call(endpoint, "/attach", {"frontend": "web"})["attachment"]
        try:
            assert_private_absent(call(endpoint, f"/snapshot?attachment={peer}"))
        finally:
            call(endpoint, "/detach", {"attachment": peer})
        assert_private_absent(call(endpoint, "/configuration/inspect", {}))
        if browser:
            assert_private_absent(browser.evaluate("JSON.stringify(sessionStorage)"))
            assert_private_absent(browser.evaluate("document.body.innerText"))
        if terminal:
            terminal.drain()
            assert_private_absent(terminal.output.decode("utf-8", errors="replace"))

    # Keep the custom author on its direct form so the two native authoring styles are covered.
    call(endpoint, "/configuration/open", {"instance": CUSTOM})
    if configuration(endpoint, CUSTOM)["effective"]["style"] != "direct":
        result = call(
            endpoint,
            "/action",
            action_request(
                endpoint,
                CUSTOM,
                "apply",
                [{"operation": "set", "path": "/style", "value": "direct"}],
            ),
        )
        assert result["status"] == "applied", result
    author_paths = {HELPER: "/label", CUSTOM: "/endpoint"}
    paths = {**author_paths, DEFAULT_MODEL: "/model"}
    adapters = ["headless", *(["browser"] if browser else []), *(["pty"] if terminal else [])]
    for adapter in adapters:
        for instance, path in paths.items():
            secret_path = "/api_key" if instance == DEFAULT_MODEL else "/token"
            call(endpoint, "/configuration/open", {"instance": instance})
            for step, token in [
                ("set", "D2_SET_CANARY"),
                ("replace", "D2_REPLACE_CANARY"),
                ("clear", None),
            ]:
                before = configuration(endpoint, instance)
                revision = call(endpoint, "/configuration/inspect", {})["revision"]
                public_value = (
                    f"https://{adapter}-{step}.invalid"
                    if instance == CUSTOM
                    else f"{adapter}-{step}"
                )
                edits = [{"operation": "set", "path": path, "value": public_value}]
                inputs = (
                    [{"operation": "clear", "path": secret_path}]
                    if token is None
                    else [{"operation": "set", "path": secret_path, "value": token}]
                )
                if adapter == "headless":
                    for action in ("validate", "preview", "apply"):
                        request = action_request(endpoint, instance, action, edits)
                        result = call(
                            endpoint, "/private-input", {"request": request, "inputs": inputs}
                        )
                        assert_private_absent(result)
                        if action == "validate":
                            assert result["errors"] == [], result
                        elif action == "preview":
                            assert result["application"] == "restart", result
                        else:
                            assert result["status"] == "applied", result
                        assert (
                            call(endpoint, "/private-input", {"request": request, "inputs": []})
                            == result
                        )
                elif adapter == "browser":
                    assert browser is not None
                    browser.action(instance, "refresh")
                    browser.action(instance, "Discard draft and use current values")
                    browser.fill_field(instance, path, public_value)
                    if token is None:
                        browser.field_action(instance, secret_path, "Clear secret")
                    else:
                        browser.fill_field(instance, secret_path, token)
                    public_surfaces()
                    browser.action(instance, "apply")
                    browser.wait(
                        "Array.from(document.querySelectorAll('input[type=password]')).every(input => input.value === '')"
                    )
                else:
                    assert terminal is not None
                    terminal.field(instance, path)
                    terminal.action(b"\x06")
                    terminal.edit(instance, path, public_value)
                    if token is None:
                        terminal.field(instance, secret_path)
                        terminal.keys(b"\x05")
                    else:
                        terminal.edit(instance, secret_path, token)
                    public_surfaces()
                    terminal.action(b"\x13")
                receipt = await_private_operation(
                    endpoint, revision, "applied", terminal if adapter == "pty" else None
                )
                receipts.append(
                    {"adapter": adapter, "instance": instance, "step": step, "receipt": receipt}
                )
                after = configuration(endpoint, instance)
                assert after["effective"][path[1:]] == public_value
                assert after["secrets_configured"][secret_path] == (token is not None)
                assert after["generation"] != before["generation"]
                assert (
                    configuration(endpoint, "note-style-context-management")["generation"]
                    == unrelated
                )
                public_surfaces()
    # Both native initializers return a diagnostic containing the submitted value.
    # The shared rollback must restore the public edit as well as the secret state.
    for instance, path in author_paths.items():
        call(endpoint, "/configuration/open", {"instance": instance})
        baseline = call(
            endpoint,
            "/private-input",
            {
                "request": action_request(endpoint, instance, "apply", []),
                "inputs": [{"operation": "set", "path": "/token", "value": "D2_REPLACE_CANARY"}],
            },
        )
        assert baseline["status"] == "applied", baseline
        before = configuration(endpoint, instance)
        request = action_request(
            endpoint,
            instance,
            "apply",
            [{"operation": "set", "path": path, "value": "must-be-rolled-back"}],
        )
        failed = call(
            endpoint,
            "/private-input",
            {
                "request": request,
                "inputs": [{"operation": "set", "path": "/token", "value": "D2_FAIL_CANARY"}],
            },
        )
        assert failed["status"] == "restored", failed
        assert call(endpoint, "/private-input", {"request": request, "inputs": []}) == failed
        after = configuration(endpoint, instance)
        assert after["effective"] == before["effective"]
        assert after["secrets_configured"] == before["secrets_configured"]
        receipts.append(
            {
                "adapter": "headless",
                "instance": instance,
                "step": "failed-restored",
                "receipt": failed,
            }
        )
        public_surfaces()
        call(endpoint, "/configuration/open", {"instance": instance})
        marker = (
            "https://private-replay-required.invalid"
            if instance == CUSTOM
            else "private-replay-required"
        )
        request = action_request(
            endpoint, instance, "apply", [{"operation": "set", "path": path, "value": marker}]
        )
        retained = call(
            endpoint,
            "/private-input",
            {
                "request": request,
                "inputs": [{"operation": "set", "path": "/token", "value": "D2_REPLAY_CANARY"}],
            },
        )
        assert retained["status"] == "applied", retained
        assert call(endpoint, "/private-input", {"request": request, "inputs": []}) == retained
        receipts.append(
            {
                "adapter": "headless",
                "instance": instance,
                "step": "retained-for-replay",
                "receipt": retained,
            }
        )
        public_surfaces()
    return {
        "scenario": "PC-06",
        "adapters": adapters,
        "receipts": receipts,
        "material_free_retry": True,
        "default_model_access": {
            "instance": DEFAULT_MODEL,
            "private_path": "/api_key",
            "provider_called": False,
        },
    }


def private_replay(
    binary: pathlib.Path, scratch: pathlib.Path, composition_file: pathlib.Path
) -> dict:
    """Reopen native authors in a fresh process; inspect and export only public records."""
    history = scratch / "history.jsonl"
    command = [
        binary,
        "--composition",
        composition_file,
        "--session",
        history,
        "--global-dir",
        scratch / "global",
        "--offline-startup",
        "--no-trust-project",
        "config",
        "inspect",
    ]
    reopened = subprocess.run(
        command, cwd=scratch, capture_output=True, text=True, timeout=30, check=False
    )
    assert_private_absent(reopened.stdout)
    assert_private_absent(reopened.stderr)
    assert reopened.returncode == 0, (reopened.stdout, reopened.stderr)
    inspection = json.loads(reopened.stdout)
    for instance in (HELPER, CUSTOM):
        item = next(item for item in inspection["instances"] if item["id"] == instance)
        assert item["secrets_configured"]["/token"]
        assert item["field_sources"]["/token"] == "explicit_session"
    snapshots = [
        json.loads(path.read_text(encoding="utf-8"))
        for path in (scratch / "global/private-input").glob("*/value.json")
    ]
    for instance in (HELPER, CUSTOM):
        assert any(
            snapshot["instance"] == instance
            and any(edit.get("value") == "D2_REPLAY_CANARY" for edit in snapshot["inputs"])
            for snapshot in snapshots
        )
    public_history = subprocess.run(
        [binary, "history", "inspect", history],
        cwd=scratch,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    assert public_history.returncode == 0, public_history.stderr
    assert_private_absent(public_history.stdout)
    assert_private_absent(public_history.stderr)
    exported = scratch / "public-export.jsonl"
    export = subprocess.run(
        [binary, "history", "export", history, exported],
        cwd=scratch,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    assert export.returncode == 0, export.stderr
    assert_private_absent(export.stdout)
    assert_private_absent(export.stderr)
    assert_private_absent(exported.read_text(encoding="utf-8"))
    return {
        "reopened_native_authors": [HELPER, CUSTOM],
        "private_references_resolved": True,
        "public_history_and_export_clean": True,
    }


def frontend_reason(mode: str) -> str | None:
    if mode == "headless":
        return "headless selected explicitly"
    reason = (
        "agent-browser is not installed"
        if not shutil.which("agent-browser")
        else "Web renderer dist is not built"
        if not (ROOT / "web/presentation/dist/index.html").is_file()
        else None
    )
    if mode == "browser" and reason is not None:
        raise RuntimeError(f"Requested browser acceptance cannot run: {reason}")
    return reason


def redact_private(text: str) -> str:
    return re.sub(r"D2_[A-Z_]*CANARY", "[private input redacted]", text)


def write_diagnostics(output: pathlib.Path, logs: dict[str, str]) -> None:
    # Preserve failure evidence without publishing the fixture's private material.
    for name, text in logs.items():
        (output / name).write_text(redact_private(text), encoding="utf-8")
    for text in logs.values():
        assert_private_absent(text)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--frontend", choices=("auto", "browser", "headless"), default="auto")
    args = parser.parse_args()
    browser_reason = frontend_reason(args.frontend)
    prepare()
    destination = installed(ROOT / "artifacts/install-configuration-forms", controlled=True)
    output = pathlib.Path(
        os.environ.get("EDEN_VERIFICATION_DIAGNOSTICS", str(ROOT / "artifacts/configuration-forms"))
    ).resolve()
    output.mkdir(parents=True, exist_ok=True)
    (output / "report.json").unlink(missing_ok=True)
    summary_file = ROOT / "artifacts/configuration-forms-verification.json"
    summary_file.unlink(missing_ok=True)
    binary = destination / "bin" / ("eden.exe" if os.name == "nt" else "eden")
    with tempfile.TemporaryDirectory(prefix="eden-configuration-forms-") as temp:
        scratch = pathlib.Path(temp)
        composition_file = scratch / "composition.json"
        composition_file.write_text(json.dumps(composition()), encoding="utf-8")
        endpoint_file = scratch / "endpoint.json"
        command = [
            binary,
            "--composition",
            composition_file,
            "--session",
            scratch / "history.jsonl",
            "--global-dir",
            scratch / "global",
            "--offline-startup",
            "--no-trust-project",
            "live",
            "--endpoint",
            endpoint_file,
            "--web-root",
            ROOT / "web/presentation/dist",
        ]
        host = subprocess.Popen(
            command, cwd=scratch, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        browser = None
        terminal = None
        try:
            deadline = time.monotonic() + 15
            while not endpoint_file.is_file() and time.monotonic() < deadline:
                assert host.poll() is None, host.communicate()
                time.sleep(0.05)
            endpoint = json.loads(endpoint_file.read_text(encoding="utf-8"))
            browser = Browser(endpoint, output) if browser_reason is None else None
            terminal = Terminal(binary, endpoint_file, endpoint) if os.name != "nt" else None
            if terminal:
                refresh_draft_evidence(endpoint, terminal)
            result = (
                exercise(endpoint, terminal, browser, output)
                if browser
                else headless(endpoint, terminal, browser_reason or "Browser unavailable")
            )
            result["private_input"] = private_evidence(endpoint, terminal, browser)
            (output / "report.json").write_text(
                json.dumps(result, indent=2) + "\n", encoding="utf-8"
            )
            if terminal:
                terminal.driver.close()
                terminal.transport.close()
            call(endpoint, "/shutdown", {})
            assert host.wait(timeout=15) == 0
            result["private_replay"] = private_replay(binary, scratch, composition_file)
            (output / "report.json").write_text(
                json.dumps(result, indent=2) + "\n", encoding="utf-8"
            )
            history = scratch / "history.jsonl"
            if history.is_file():
                records = [
                    record
                    for line in history.read_text(encoding="utf-8").splitlines()
                    for record in json.loads(line).get("transaction", [json.loads(line)])
                ]
                assert all(
                    view.get("scope") != "management"
                    for record in records
                    if record["kind"] == "presentation_static"
                    for view in record["payload"]["views"]
                )
            summary = {
                "status": "passed",
                "browser": result["browser"],
                "requested_frontend": args.frontend,
                "pty": result["pty"],
                "browser_skip_reason": result.get("browser_skip_reason"),
                "pty_skip_reason": None if terminal else "POSIX PTY unavailable on Windows",
                "scenarios": [
                    "PC-01",
                    "PC-02",
                    "PC-03",
                    "PC-04",
                    "PC-06",
                    "PC-08",
                    "notes-and-cache-warmer",
                    "default-coding-json-fallback",
                ]
                if browser
                else [
                    "native-form-validation-preview-apply",
                    "PC-06-headless-private-input",
                    "native-business-configuration",
                    *(["pty-repeat-apply"] if terminal else []),
                ],
                "detail": str(output / "report.json"),
            }
            summary_file.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
            print(
                "PASS: installed native configuration forms; "
                + ("real Chromium" if browser else f"browser skipped: {browser_reason}")
                + ("; real PTY" if terminal else "; PTY unavailable on Windows")
            )
        finally:
            try:
                if browser:
                    browser.close()
            finally:
                try:
                    if terminal:
                        terminal.transport.snapshots.set()
                        try:
                            terminal.driver.close()
                        finally:
                            terminal.transport.close()
                finally:
                    if host.poll() is None:
                        host.kill()
                    stdout, stderr = host.communicate()
                    logs = {"host.stdout.log": stdout, "host.stderr.log": stderr}
                    if terminal:
                        logs["tui.ansi"] = terminal.output.decode("utf-8", errors="replace")
                        logs["tui-actions.json"] = json.dumps(terminal.transport.receipts, indent=2)
                    write_diagnostics(output, logs)


def cli() -> None:
    try:
        main()
    except Exception as error:
        # Cleanup failures can embed terminal bytes in a chained exception. Sanitize
        # that final stderr boundary too, before the runner captures command logs.
        print(redact_private("".join(traceback.format_exception(error))), file=sys.stderr, end="")
        raise SystemExit(1) from None


if __name__ == "__main__":
    cli()
