#!/usr/bin/env python3
"""Exercise one external native presentation author through a separate live host process."""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

from install import ROLES, ROOT, build_target, library, package, target
from verification import author_artifact, installed, prepare

ACTION = "eden.presentation.action.v1"
CODING = "eden.coding-loop.v2"
CODING_CONTEXT = "eden.coding-context.v2"
CODING_PROVIDER = "eden.coding-provider.v1"
CODING_TOOL = "eden.coding-tool.v1"
QUEUE = "eden.submission-queue.v2"
STORE = "eden.session-store.v2"
PROVIDES = [
    ROLES[0],
    CODING_CONTEXT,
    CODING_PROVIDER,
    CODING_TOOL,
    QUEUE,
    CODING,
    ACTION,
    *ROLES[1:],
]


def call(endpoint: dict, route: str, body: dict | None = None) -> dict:
    request = urllib.request.Request(
        f"http://{endpoint['address']}{route}",
        data=None if body is None else json.dumps(body).encode(),
        headers={"X-Eden-Token": endpoint["token"], "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=25) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        result = json.load(error)
    if not result["ok"]:
        raise RuntimeError(result["error"])
    return result["result"]


def wait_for(endpoint: dict, predicate, seconds: float = 10.0) -> dict:
    deadline = time.monotonic() + seconds
    sequence = None
    while time.monotonic() < deadline:
        route = "/snapshot" if sequence is None else f"/snapshot?after={sequence}"
        frame = call(endpoint, route)
        sequence = frame["presentation"]["sequence"]
        if predicate(frame):
            return frame
    raise AssertionError("live snapshot did not reach the expected state")


def tui_keyboard_probe(binary: pathlib.Path, endpoint_file: pathlib.Path, endpoint: dict) -> bool:
    """Drive the installed Ratatui adapter through a real Unix PTY, including option ten."""
    if os.name == "nt":
        return False
    import fcntl
    import pty
    import select
    import struct
    import termios

    run = call(endpoint, "/prompt", {"request_id": "tui-keyboard", "text": "terminal review"})[
        "run_id"
    ]
    wait_for(
        endpoint,
        lambda frame: any(
            view["id"] == "review" and view["active"] for view in frame["presentation"]["views"]
        ),
    )
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))
    client = subprocess.Popen(
        [binary, "live-tui", "--endpoint", endpoint_file],
        cwd=endpoint_file.parent,
        stdin=slave,
        stdout=slave,
        stderr=subprocess.PIPE,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    os.close(slave)
    try:
        frame = bytearray()
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline and b"Reason" not in frame:
            ready, _, _ = select.select([master], [], [], 0.2)
            if ready:
                frame.extend(os.read(master, 65536))
        assert b"External tool review" in frame and b"fn old()" in frame and b"Reason" in frame
        os.write(master, b"\t")
        time.sleep(0.15)
        os.write(master, b"terminal confirmed")
        time.sleep(0.15)
        os.write(master, b"\t")
        time.sleep(0.15)
        assert call(endpoint, "/snapshot")["presentation"]["activity"] == []
        os.write(master, b"." * 9)
        os.write(master, b" ")
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                frame.extend(os.read(master, 65536))
            if call(endpoint, "/snapshot")["presentation"]["activity"]:
                break
        else:
            raise AssertionError(("PTY did not toggle the multi-choice field", client.poll()))
        os.write(master, b"\r")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                frame.extend(os.read(master, 65536))
            if call(endpoint, "/snapshot")["state"]["active_run"] is None:
                break
        else:
            raise AssertionError(
                ("PTY Enter did not settle the run", client.poll(), bytes(frame[-500:]))
            )
        terminal = call(endpoint, "/terminal", {"run_id": run})
        assert terminal["outcome"] == {
            "status": "completed",
            "value": {"reason": "terminal confirmed", "checks": ["release"]},
        }, terminal
        os.write(master, b"\x1b")
        deadline = time.monotonic() + 5
        while client.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    os.read(master, 65536)
                except OSError:
                    break
        assert client.wait(timeout=1) == 0
        assert run > 0
        return True
    finally:
        if client.poll() is None:
            client.kill()
            client.wait()
        os.close(master)


def read_tui_probe(binary: pathlib.Path, endpoint_file: pathlib.Path) -> bool:
    """Open the saved result through the same terminal renderer without its plugin."""
    if os.name == "nt":
        return False
    import fcntl
    import pty
    import select
    import struct
    import termios

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))
    client = subprocess.Popen(
        [binary, "live-tui", "--endpoint", endpoint_file],
        cwd=endpoint_file.parent,
        stdin=slave,
        stdout=slave,
        stderr=subprocess.PIPE,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    os.close(slave)
    try:
        output = bytearray()
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.2)
            if ready:
                output.extend(os.read(master, 65536))
            if b"read-only" in output and b"fn old()" in output and b"src/main.rs" in output:
                break
        else:
            raise AssertionError(("saved TUI content missing", bytes(output[-800:])))
        os.write(master, b"")
        while client.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    os.read(master, 65536)
                except OSError:
                    break
        assert client.wait(timeout=1) == 0
        return True
    finally:
        if client.poll() is None:
            client.kill()
            client.wait()
        os.close(master)


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/install-presentation", controlled=True)
    with tempfile.TemporaryDirectory(prefix="eden-presentation-") as temp:
        scratch = pathlib.Path(temp)
        binary = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
        native = scratch / author_artifact("presentation-live").name
        shutil.copy2(author_artifact("presentation-live"), native)
        composition = {
            "packages": [
                package("presentation-live", PROVIDES, str(native), target()),
                package(
                    "local-history",
                    [STORE],
                    str(build_target() / "debug" / library("eden_local_history")),
                    target(),
                ),
            ],
            "roles": {
                **{role: "presentation-live" for role in ROLES},
                CODING: "presentation-live",
                CODING_CONTEXT: "presentation-live",
                CODING_PROVIDER: "presentation-live",
                CODING_TOOL: "presentation-live",
                QUEUE: "presentation-live",
                STORE: "local-history",
            },
            "resource_packages": [],
        }
        composition_file = scratch / "composition.json"
        composition_file.write_text(json.dumps(composition), encoding="utf-8")
        endpoint_file = scratch / "endpoint.json"
        history_file = scratch / "history.jsonl"
        command = [
            binary,
            "--composition",
            composition_file,
            "--session",
            history_file,
            "--offline-startup",
            "--no-trust-project",
            "live",
            "--endpoint",
            endpoint_file,
        ]
        web_root = ROOT / "web/presentation/dist"
        if web_root.is_dir():
            command.extend(["--web-root", web_root])
        host = subprocess.Popen(
            command, cwd=scratch, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        try:
            for _ in range(100):
                if endpoint_file.is_file():
                    break
                if host.poll() is not None:
                    raise AssertionError(host.communicate())
                time.sleep(0.05)
            endpoint = json.loads(endpoint_file.read_text(encoding="utf-8"))
            tui = call(endpoint, "/attach", {"frontend": "tui"})["attachment"]
            web = call(endpoint, "/attach", {"frontend": "web"})["attachment"]
            run = call(endpoint, "/prompt", {"request_id": "start", "text": "show"})["run_id"]
            assert (
                call(endpoint, "/prompt", {"request_id": "start", "text": "show"})["run_id"] == run
            )
            frame = wait_for(
                endpoint,
                lambda frame: any(
                    view["id"] == "review" and view["revision"] >= 2
                    for view in frame["presentation"]["views"]
                ),
            )
            view = next(view for view in frame["presentation"]["views"] if view["id"] == "review")
            assert view["owner"] == "presentation-live"
            assert {node["kind"] for node in view["nodes"]} >= {
                "text",
                "table",
                "diff",
                "attachment",
                "form",
                "status",
            }
            assert next(
                item for item in frame["presentation"]["views"] if item["id"] == "terminal-helper"
            )["platforms"] == ["tui"]
            call(
                endpoint,
                "/activity",
                {"attachment": tui, "target": {"kind": "composer"}, "active": True},
            )
            assert len(call(endpoint, "/snapshot")["presentation"]["activity"]) == 1
            call(endpoint, "/detach", {"attachment": tui})
            call(endpoint, "/detach", {"attachment": web})
            assert call(endpoint, "/snapshot")["state"]["active_run"] == run
            reconnected = call(endpoint, "/attach", {"frontend": "web"})["attachment"]
            assert (
                next(
                    item
                    for item in call(endpoint, "/snapshot")["presentation"]["views"]
                    if item["id"] == "review"
                )["revision"]
                == view["revision"]
            )
            action = {
                "session_id": endpoint["session_id"],
                "owner": view["owner"],
                "view_id": view["id"],
                "revision": view["revision"],
                "action": "approve",
                "request_id": "review-1",
                "values": {"reason": "approved", "checks": ["tests"]},
            }
            try:
                call(
                    endpoint,
                    "/action",
                    {**action, "request_id": "invalid", "values": {"reason": "", "checks": []}},
                )
                raise AssertionError("invalid form was accepted")
            except RuntimeError as error:
                assert "InvalidInput" in str(error)
            result = call(endpoint, "/action", action)
            assert result["calls"] == 1
            assert call(endpoint, "/action", action) == result
            try:
                call(endpoint, "/action", {**action, "request_id": "stale", "revision": 0})
                raise AssertionError("stale action was accepted")
            except RuntimeError as error:
                assert "StaleRevision" in str(error) or "Unavailable" in str(error)
            wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
            call(endpoint, "/detach", {"attachment": reconnected})
            run = call(endpoint, "/prompt", {"request_id": "cancel-run", "text": "again"})["run_id"]
            wait_for(
                endpoint,
                lambda frame: any(
                    view["id"] == "review" and view["active"]
                    for view in frame["presentation"]["views"]
                ),
            )
            call(endpoint, "/cancel", {"run_id": run})
            wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
            late = call(
                endpoint, "/prompt", {"request_id": "late-dialog", "text": "legacy-after-detach"}
            )["run_id"]
            frame = wait_for(endpoint, lambda frame: frame["presentation"]["pending_interactions"])
            assert frame["state"]["active_run"] == late
            dialog = next(
                view
                for view in frame["presentation"]["views"]
                if view["owner"] == "eden-host-interaction" and view["active"]
            )
            joined = call(endpoint, "/attach", {"frontend": "tui"})["attachment"]
            answer = {
                "session_id": endpoint["session_id"],
                "owner": dialog["owner"],
                "view_id": dialog["id"],
                "revision": dialog["revision"],
                "action": "respond",
                "request_id": "late-answer",
                "values": {"value": True},
            }
            assert call(endpoint, "/action", answer) == {"delivered": True}
            wait_for(endpoint, lambda frame: frame["state"]["active_run"] is None)
            call(endpoint, "/detach", {"attachment": joined})
            tui_keyboard = tui_keyboard_probe(binary, endpoint_file, endpoint)
            call(endpoint, "/shutdown", {})
            assert host.wait(timeout=15) == 0
            assert history_file.is_file()
            records = [
                record
                for line in history_file.read_text(encoding="utf-8").splitlines()
                for record in json.loads(line).get("transaction", [json.loads(line)])
            ]
            saved = [record for record in records if record["kind"] == "presentation_static"]
            assert len(saved) >= 2, [record["kind"] for record in records]
            assert any(
                node["kind"] == "diff"
                for record in saved
                for view in record["payload"]["views"]
                for node in view["nodes"]
            )
            native.unlink()
            shutil.copy2(history_file, ROOT / "artifacts/presentation-history.jsonl")
            read_endpoint_file = scratch / "read-endpoint.json"
            read_command = [
                binary,
                "read",
                history_file,
                "--endpoint",
                read_endpoint_file,
            ]
            if web_root.is_dir():
                read_command.extend(["--web-root", web_root])
            reader = subprocess.Popen(
                read_command, cwd=scratch, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
            )
            try:
                for _ in range(100):
                    if read_endpoint_file.is_file():
                        break
                    if reader.poll() is not None:
                        raise AssertionError(reader.communicate())
                    time.sleep(0.05)
                read_endpoint = json.loads(read_endpoint_file.read_text(encoding="utf-8"))
                static = call(read_endpoint, "/snapshot")
                assert static["state"]["read_only"] is True
                assert all(not view["active"] for view in static["presentation"]["views"])
                assert any(
                    node["kind"] == "diff"
                    for view in static["presentation"]["views"]
                    for node in view["nodes"]
                )
                saved_tui = read_tui_probe(binary, read_endpoint_file)
                for route, body in (
                    ("/action", action),
                    ("/prompt", {"request_id": "read-only", "text": "run"}),
                ):
                    try:
                        call(read_endpoint, route, body)
                        raise AssertionError(f"{route} executed in read-only history")
                    except RuntimeError as error:
                        assert "Unsupported" in str(error)
                call(read_endpoint, "/shutdown", {})
                assert reader.wait(timeout=15) == 0
            finally:
                if reader.poll() is None:
                    reader.kill()
                    reader.wait()
            evidence = {
                "session_id": endpoint["session_id"],
                "view_revision": view["revision"],
                "action_calls": result["calls"],
                "cancelled_run": run,
                "late_dialog_run": late,
                "host_exit": 0,
                "tui_keyboard": tui_keyboard,
                "static_views": len(static["presentation"]["views"]),
                "plugin_removed_read_only": True,
                "saved_tui": saved_tui,
            }
            artifact = ROOT / "artifacts/presentation-verification.json"
            artifact.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
            print(json.dumps(evidence))
        finally:
            if host.poll() is None:
                host.kill()
                host.wait()
            if host.returncode and host.stderr is not None and not host.stderr.closed:
                print(host.stderr.read(), file=sys.stderr)


if __name__ == "__main__":
    main()
