#!/usr/bin/env python3
"""Installed product workflows through the same loopback host used by the terminal."""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from install import ROOT, package, target
from verification import author_artifact, installed, prepare


def load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / filename)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> None:
    prepare()
    live = load("workflow_live", "verify-tui.py")
    models = load("workflow_models", "verify-model-access.py")
    results: dict[str, Any] = {"status": "passed", "platform": sys.platform}
    with tempfile.TemporaryDirectory(prefix="eden-workflows-") as directory:
        root = Path(directory)
        installation = installed(root / "install")
        binary = installation / "bin" / ("eden.exe" if os.name == "nt" else "eden")
        composition = installation / "composition.json"
        config = json.loads(composition.read_text())
        server = models.Server()
        history = root / "conversation.jsonl"
        endpoint_file = root / "host.json"
        hosts = []
        try:
            (root / "proof.txt").write_text(models.MARKER)
            for item in config["packages"]:
                if item["descriptor"]["package"] == "model-access":
                    item["config"] = {
                        "catalog": {"models": [models.model("openai-completions", server.base)]},
                        "credentials": {"path": str(root / "global/keys.json")},
                    }
            roles = ["eden.exporter.v1", "eden.share-target.v1", "eden.update-source.v1"]
            config["packages"].append(
                package(
                    "delivery-services",
                    [*roles, "eden.instance-stop.v1"],
                    str(author_artifact("delivery-services")),
                    target(),
                )
            )
            for role in roles[1:]:
                config["roles"][role] = "delivery-services"
            composition.write_text(json.dumps(config))
            host = subprocess.Popen(
                [
                    str(binary),
                    "--composition",
                    str(composition),
                    "--cwd",
                    str(root),
                    "--global-dir",
                    str(root / "global"),
                    "--session",
                    str(history),
                    "--offline-startup",
                    "live",
                    "--endpoint",
                    str(endpoint_file),
                ],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "EDEN_TUI_STATE_DIR": str(root / "host-state")},
            )
            endpoint = live.ready(host, endpoint_file)
            hosts.append((host, endpoint))
            counter = 0

            def call(route, body=None):
                return live.call(
                    endpoint,
                    route,
                    None if route == "/tui/snapshot" else ({} if body is None else body),
                )

            def mutate(route, body):
                nonlocal counter
                counter += 1
                request = {**body, "request_id": f"workflow-{counter}"}
                result = call(route, request)
                assert call(route, request) == result, "repeated request changed its receipt"
                if "run_id" in result:
                    terminal = call("/terminal", {"run_id": result["run_id"]})
                    assert terminal["outcome"]["status"] == "completed", terminal
                    assert not terminal["cleanup_errors"], terminal
                    return terminal["outcome"]["value"]
                return result

            catalog = call("/models/list")
            assert "fixture" in catalog["providers"]
            challenge = mutate(
                "/auth/start", {"request": {"action": "start", "provider": "fixture"}}
            )
            key_run = call(
                "/auth/input",
                {
                    "operation_id": challenge["operation_id"],
                    "api_key": True,
                    "input": models.SECRET,
                },
            )
            assert call("/terminal", key_run)["outcome"]["value"]["status"] == "completed"
            target_model = {
                "provider": "fixture",
                "model": "openai-completions",
                "thinking": "high",
            }
            selected = mutate("/models/select", {"selection": target_model})
            assert selected["thinking"]["requested"] == "high"
            assert call("/models/current")["selection"] == target_model
            mutate("/prompt", {"text": "WORKFLOW_USER_MARKER"})
            mutate("/manage/metadata", {"name": "Workflow session", "tags": ["workflow"]})
            entries = call("/manage/sessions", {"directory": str(root)})
            selected_session = next(e for e in entries if e["path"] == str(history))
            assert selected_session["name"] == "Workflow session"
            try:
                mutate(
                    "/manage/delete",
                    {
                        "path": str(history),
                        "expected_session": selected_session["session_id"],
                        "confirmed": True,
                    },
                )
            except RuntimeError as error:
                assert "active" in str(error)
            else:
                raise AssertionError("active session was deleted")
            copied = root / "copy.jsonl"
            preview = mutate(
                "/manage/copy/preview",
                {"source": str(history), "destination": str(copied), "kind": "clone"},
            )
            mutate("/manage/copy/apply", {"preview_id": preview["preview_id"], "confirmed": True})
            assert copied.exists()
            mutate("/manage/rename", {"path": str(copied), "name": "Copied", "tags": []})
            export = mutate("/delivery/preview", {"selection": {"tools": False, "thinking": False}})
            assert "WORKFLOW_USER_MARKER" in export["artifact"]["content"]
            mutate("/manage/metadata", {"name": "Grown after export", "tags": []})
            saved = root / "reading.jsonl"
            mutate("/delivery/save", {"preview_id": export["preview_id"], "path": str(saved)})
            assert saved.read_text() == export["artifact"]["content"]
            published = mutate(
                "/delivery/publish", {"preview_id": export["preview_id"], "confirmed": True}
            )
            assert published["content"] == export["artifact"]["content"]
            assert published["url"] == "https://example.invalid/author-target"
            assert (
                mutate("/updates", {"request": {"operation": "discover"}})["targets"][0][
                    "current_version"
                ]
                == "external-source"
            )
            assert call("/configuration/inspect")["instances"]
            assert isinstance(call("/resources"), dict)
            assert Path(call("/trust/inspect")["cwd"]).samefile(root)
            reader = mutate("/manage/open", {"path": str(saved), "read_only": True})
            reader_endpoint = json.loads(Path(reader["endpoint"]).read_text())
            reading_frame = live.call(reader_endpoint, "/tui/snapshot")
            assert reading_frame["state"]["read_only"] and reading_frame["history"] == []
            assert "WORKFLOW_USER_MARKER" in json.dumps(reading_frame["reading"])
            try:
                live.call(
                    reader_endpoint,
                    "/prompt",
                    {"request_id": "cannot-restore", "text": "forbidden"},
                )
            except RuntimeError as error:
                assert "read-only" in str(error)
            else:
                raise AssertionError("reading artifact accepted execution")
            if os.name != "nt":
                from tui_pty import Terminal

                reader_tui = Terminal(binary, Path(reader["endpoint"]))
                try:
                    reader_tui.wait("WORKFLOW_USER_MARKER")
                finally:
                    reader_tui.close()
            live.call(reader_endpoint, "/shutdown", {})
            results["reading_jsonl_read_only_and_pty"] = True
            snapshot = call("/tui/snapshot")
            assert (
                models.SECRET not in json.dumps(snapshot) + history.read_text() + saved.read_text()
            )
            second = mutate("/manage/open", {"path": str(copied), "read_only": False})
            second_endpoint = json.loads(Path(second["endpoint"]).read_text())
            returned = live.call(
                second_endpoint,
                "/manage/open",
                {"path": str(history), "read_only": False, "request_id": "return-original"},
            )
            assert returned["attached_existing"] and Path(returned["endpoint"]).samefile(
                endpoint_file
            )
            assert (
                live.call(second_endpoint, "/tui/snapshot")["state"]["session_id"]
                == preview["plan"]["new_session"]
            )
            results["existing_live_session_return"] = True
            results["models_auth_sessions_copy_export_share_updates"] = True
            results["private_input_excluded_from_public_history"] = True
            if os.name != "nt":
                from tui_pty import Terminal

                terminal = Terminal(binary, endpoint_file)
                try:
                    terminal.wait("Connected")
                    for command, title in [
                        ("/models", "Models"),
                        ("/settings", "Settings"),
                        ("/plugins", "Plugins"),
                        ("/resources", "Resources"),
                        ("/background", "Notes and cache warming"),
                        ("/delivery", "Export and sharing"),
                        ("/updates", "Updates"),
                    ]:
                        terminal.command(command)
                        terminal.wait(title)
                        terminal.send(b"\x1b")
                    terminal.command("/sessions")
                    terminal.wait("Browse another directory")
                    terminal.send(b"\x1b[B\x1b[B\r")
                    terminal.wait("Saved session directory")
                    terminal.send(b"\x15" + str(root).encode() + b"\x13")
                    terminal.wait("Grown after export")
                    terminal.send(b"Copied\r")
                    terminal.wait("Resume stopped history")
                    terminal.send(b"\r")
                    terminal.wait(str(preview["plan"]["new_session"]))
                    # The copied session identity must be visible before the next action.
                    terminal.command("/tree")
                    terminal.wait("Session tree")
                    terminal.send(b"\x1b")
                    terminal.command("/stop-host")
                    terminal.process.wait(timeout=10)
                finally:
                    terminal.close()
                results["management_pty_and_explicit_switch"] = True
            else:
                results["management_pty_and_explicit_switch"] = "skipped: POSIX PTY unavailable"
        finally:
            for host, endpoint in hosts:
                live.stop(host, endpoint)
            for child_endpoint in (root / "host-state/hosts").glob("*.json"):
                if child_endpoint.exists():
                    try:
                        live.call(json.loads(child_endpoint.read_text()), "/shutdown", {})
                    except (OSError, RuntimeError):
                        pass
                    deadline = time.monotonic() + 10
                    while child_endpoint.exists() and time.monotonic() < deadline:
                        time.sleep(0.02)
                    assert not child_endpoint.exists(), "switched host did not finish shutdown"
            server.close()
    destination = ROOT / "artifacts/product-workflows-verification.json"
    destination.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results))


if __name__ == "__main__":
    main()
