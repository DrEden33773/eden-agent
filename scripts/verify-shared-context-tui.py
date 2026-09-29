#!/usr/bin/env python3
"""Installed shared-context TUI through a real POSIX PTY and controlled local provider.

Default preparation uses the frozen verification installation. Explicit --binary and
--composition exercise those bytes without installation; --label identifies an old
seed during development. No external model endpoint or real credential is used.
"""

import argparse
import base64
import copy
import http.server
import importlib.util
import json
import os
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib
from collections.abc import Callable
from pathlib import Path
from typing import Any

from http_fixture import FixtureHTTPServer
from install import ROOT
from tui_pty import Terminal
from verification import installed, prepare


def load_script(name: str, filename: str) -> Any:
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / filename)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


presentation = load_script("shared_context_tui_presentation", "verify-presentation.py")
fixture = load_script("shared_context_tui_provider", "verify-context.py")
call = presentation.call


def wait_until(predicate: Callable[[], Any], description: str, seconds: float = 15) -> Any:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.03)
    raise AssertionError(f"Timed out waiting for {description}")


def ready(process: subprocess.Popen, path: Path) -> dict:
    def published():
        assert process.poll() is None, process.communicate()
        if path.exists():
            return json.loads(path.read_text(encoding="utf-8"))
        return None

    return wait_until(published, "live endpoint publication")


def png() -> str:
    """A small opaque raster needs no external image tools or downloaded assets."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
        )

    image = b"\x89PNG\r\n\x1a\n"
    image += chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 2, 8, 6, 0, 0, 0))
    image += chunk(b"IDAT", zlib.compress((b"\x00" + bytes([220, 50, 80, 255]) * 4) * 2))
    image += chunk(b"IEND", b"")
    return base64.b64encode(image).decode()


class CompletionGate:
    """Relay the real host; hold only a management terminal receipt for UI observation."""

    def __init__(self, upstream: dict):
        self.preview_hold = False
        self.preview_requested = threading.Event()
        self.preview_release = threading.Event()
        self.hold = False
        self.requested = threading.Event()
        self.release = threading.Event()
        self.run_id: int | None = None
        self.errors: list[str] = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_GET(self) -> None:
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    body = json.loads(self.rfile.read(size)) if size else None
                    result = call(upstream, self.path, body)
                    if (
                        owner.hold
                        and self.path == "/terminal"
                        and body
                        and body.get("context_operation")
                    ):
                        owner.run_id = body["run_id"]
                        owner.requested.set()
                        assert owner.release.wait(15), "management completion gate not released"
                    if owner.preview_hold and self.path == "/reference/preview":
                        owner.preview_requested.set()
                        assert owner.preview_release.wait(15), "source preview gate not released"
                    output = json.dumps({"ok": True, "result": result}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(output)))
                    self.end_headers()
                    self.wfile.write(output)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except BaseException as error:
                    owner.errors.append(repr(error))
                    self.send_error(500)

            do_POST = do_GET

        self.http = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        self.endpoint = {**upstream, "address": f"127.0.0.1:{self.http.server_port}"}
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.release.set()
        self.preview_release.set()
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()
        assert not self.errors, self.errors


def history(endpoint: dict) -> list[dict]:
    return call(endpoint, "/tui/snapshot")["history"]


def wait_record(endpoint: dict, kind: str) -> dict:
    return wait_until(
        lambda: next((r for r in reversed(history(endpoint)) if r["kind"] == kind), None),
        f"committed {kind} record",
    )


def draft(state: Path) -> dict | None:
    for path in state.glob("draft-*.json"):
        try:
            saved = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if saved["draft"].get("references"):
            return saved["draft"]
    return None


def quit_preserving(terminal: Terminal) -> None:
    """Normal /quit persists references; Terminal.close alone clears the composer first."""
    terminal.command("/quit")
    wait_until(lambda: terminal.process.poll() is not None, "normal TUI detach")
    terminal.close()


def exercise(binary: Path, composition: Path, scratch: Path) -> dict:
    project = scratch / "project"
    sources = project / ".eden/sessions"
    sources.mkdir(parents=True)
    global_dir = scratch / "global"
    global_dir.mkdir()
    fixture.write(
        global_dir / "settings.json", {"discover_skills": False, "discover_templates": False}
    )
    server = fixture.Server(
        lambda _body, _index: (200, fixture.complete(fixture.answer("CONTROLLED-ANSWER")))
    )
    host: subprocess.Popen | None = None
    gate: CompletionGate | None = None
    terminal: Terminal | None = None
    endpoint: dict = {}
    try:
        configured = json.loads(composition.read_text(encoding="utf-8"))
        for item in configured["packages"]:
            native = Path(item["library"])
            if not native.is_absolute():
                item["library"] = str((composition.resolve().parent / native).resolve())
        config = fixture.configure(scratch, configured, server, "tui-controlled")
        endpoint_file = scratch / "host.json"
        session_file = scratch / "target.jsonl"
        host = subprocess.Popen(
            [
                str(binary),
                "--composition",
                str(config),
                "--cwd",
                str(project),
                "--global-dir",
                str(global_dir),
                "--session",
                str(session_file),
                "--offline-startup",
                "--no-trust-project",
                "live",
                "--endpoint",
                str(endpoint_file),
            ],
            cwd=project,
            env={
                **os.environ,
                "EDEN_CONTEXT_KEY": "context-verifier-key",
                "EDEN_AGENT_DIR": str(global_dir),
            },
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        endpoint = ready(host, endpoint_file)
        image = png()
        run_id = call(
            endpoint,
            "/prompt",
            {
                "request_id": "tui-seed",
                "content": [
                    {"type": "text", "text": "SOURCE-ORIGINAL"},
                    {"type": "image", "media_type": "image/png", "data": image},
                ],
            },
        )["run_id"]
        outcome = call(endpoint, "/terminal", {"run_id": run_id})
        assert outcome["outcome"]["status"] == "completed", outcome
        image_record = wait_record(endpoint, "image_version")
        original = copy.deepcopy(history(endpoint))
        original_bytes = session_file.read_bytes()
        source = sources / "00-reference-source.jsonl"
        source.write_bytes(session_file.read_bytes())
        (sources / "99-unrelated.jsonl").write_bytes(session_file.read_bytes())
        initial_source = source.read_bytes()
        gate = CompletionGate(endpoint)
        proxy_file = scratch / "frontend.json"
        fixture.write(proxy_file, gate.endpoint)
        state = scratch / "tui-state"
        environment = {
            "EDEN_TUI_STATE_DIR": str(state),
            "TERM_SESSION_ID": "shared-context-tui-verification",
            "TMUX": "verification-raster-fallback",
        }
        terminal = Terminal(binary, proxy_file, env=environment, width=120, height=38)
        terminal.wait("Connected")
        terminal.wait("▀")
        assert "Context ·" not in terminal.display, "pixels only appeared inside inspector"
        terminal.resize(60, 18)
        terminal.wait("▀")
        terminal.resize(120, 38)
        terminal.wait("Connected")
        terminal.command("/context")
        terminal.wait("Captured shared model input")
        captured = call(endpoint, "/context/inspect", {})
        user_index = next(
            i
            for i, entry in enumerate(captured["effective"]["entries"])
            if "SOURCE-ORIGINAL" in json.dumps(entry["item"])
        )
        terminal.send(b"\x1b[B" * user_index + b"e")
        terminal.wait("Editing local draft")
        terminal.send(b"\x01\x0bEDITED-BY-REAL-TUI\x13")
        terminal.send(b"p")
        terminal.wait("Structure valid")
        terminal.send(b"a")
        edit_record = wait_record(endpoint, "context_edit")
        terminal.wait("Applied")
        assert "EDITED-BY-REAL-TUI" in json.dumps(
            call(endpoint, "/context/inspect", {})["effective"]
        )
        assert history(endpoint)[: len(original)] == original, (
            "context edit rewrote original history"
        )
        terminal.send(b"I")
        terminal.wait("Active sent version")
        terminal.wait("Decoded 4×2")
        assert "▀".encode() in terminal.output, "no raster image cells emitted"
        terminal.send(b"o")
        terminal.wait("Original")
        terminal.wait("Decoded 4×2")
        terminal.resize(60, 18)
        terminal.wait("Context")
        terminal.resize(120, 38)
        terminal.wait("Decoded 4×2")
        terminal.send(b"\x1b")
        terminal.command("/context")
        terminal.wait("Context")
        terminal.send(b"\t" * 1)
        # Use the document view before editing; unsaved local text must survive rebuild.
        for _ in range(8):
            if "Effective draft" in terminal.display.splitlines()[0]:
                break
            terminal.send(b"\t")
        else:
            raise AssertionError(
                f"Context views did not return to effective draft:\n{terminal.display}"
            )
        terminal.send(b"\x1b[B" * user_index + b"e")
        terminal.wait("Editing local draft")
        terminal.send(b"\x01\x0bUNSAVED-REBUILD-DRAFT\x13")
        terminal.send(b"R")
        terminal.wait("Rebuild options")
        before_rebuild_requests = len(server.requests)
        gate.hold = True
        terminal.send(b"\x13")
        assert gate.requested.wait(10), "TUI did not wait for the admitted management run"
        terminal.wait("accepted")
        assert "Operation completed" not in terminal.display
        assert gate.run_id is not None
        gate.release.set()
        terminal.wait("Operation completed")
        terminal.wait("result refreshed")
        rebuilt = wait_record(endpoint, "context_rebuild")
        assert len(server.requests) == before_rebuild_requests, "rebuild called a model"
        assert session_file.read_bytes().startswith(original_bytes), (
            "rebuild rewrote original records"
        )
        terminal.send(b"\t\t\t")
        terminal.wait("Effective draft")
        terminal.wait("UNSAVED-REBUILD-DRAFT")
        terminal.send(b"\x1b")
        terminal.send(b"@session\t")
        terminal.wait("choose a saved session")
        terminal.wait("2 of 2 sessions")
        terminal.send(b"00-reference-source")
        terminal.wait("1 of 2 sessions")
        terminal.send(b"\r")
        terminal.wait("choose a source branch")
        gate.preview_hold = True
        terminal.send(b"\r")
        assert gate.preview_requested.wait(10), "source preview was not requested"
        terminal.send(b"\x1b")
        terminal.send(b"\x03@session\t")
        terminal.wait("choose a saved session")
        terminal.send(b"00-reference-source")
        terminal.wait("1 of 2 sessions")
        gate.preview_hold = False
        gate.preview_release.set()
        terminal.read(0.3)
        assert "choose a saved session" in terminal.display
        assert "fixed source preview" not in terminal.display, "late source reply replaced search"
        terminal.send(b"\r")
        terminal.wait("choose a source branch")
        terminal.send(b"\r")
        terminal.wait("fixed source preview")
        terminal.wait("[ ] Image")
        terminal.send(b"\x1b[B ")
        terminal.wait("[x] Image")
        terminal.send(b"i")
        terminal.wait("Frozen session reference inserted")
        frozen_draft = wait_until(lambda: draft(state), "persisted frozen reference")
        frozen = copy.deepcopy(frozen_draft["references"][0])
        assert len(frozen["included_images"]) == 1
        assert any(block.get("data") == image for block in frozen["content"])
        source_records = fixture.records(source)
        grown = {
            "schema_version": 2,
            "session_id": source_records[0]["session_id"],
            "sequence": max(r["sequence"] for r in source_records) + 1,
            "parent_id": frozen["source"]["head"],
            "branch": frozen["source"]["branch"],
            "run_id": 0,
            "kind": "message",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "text", "text": "SOURCE-GREW-LATER"}],
            },
        }
        with source.open("ab") as output:
            output.write(
                (json.dumps({"schema_version": 2, "transaction": [grown]}) + "\n").encode()
            )
        assert source.read_bytes().startswith(initial_source)
        assert "SOURCE-GREW-LATER" in json.dumps(
            call(endpoint, "/reference/preview", {"path": str(source)})
        )
        saved = draft(state)
        assert saved is not None and saved["references"][0] == frozen
        quit_preserving(terminal)
        assert host.poll() is None, "frontend detach stopped shared host"
        terminal = Terminal(binary, proxy_file, env=environment, width=120, height=38)
        terminal.wait("Connected")
        terminal.command("/recover")
        terminal.command("/references")
        terminal.wait("attached draft snapshots")
        terminal.wait("SOURCE-ORIGINAL")
        assert "SOURCE-GREW-LATER" not in terminal.display
        terminal.send(b"\x1b")
        saved = draft(state)
        assert saved is not None and saved["references"][0] == frozen
        terminal.command("Use this fixed reference")
        wait_until(
            lambda: len(server.requests) > before_rebuild_requests, "controlled referenced request"
        )
        wait_until(
            lambda: call(endpoint, "/snapshot")["state"]["active_run"] is None,
            "referenced run completion",
        )
        sent = json.dumps(server.requests[-1]["input"])
        assert "SOURCE-ORIGINAL" in sent and "SOURCE-GREW-LATER" not in sent
        assert image in sent, "explicit reference image did not reach provider request"
        terminal.close()
        terminal = None
        return {
            "context_edit_record": edit_record["sequence"],
            "original_prefix_preserved": True,
            "rebuild_admission_run": gate.run_id,
            "rebuild_record": rebuilt["sequence"],
            "completion_receipt_gated": True,
            "rebuild_no_model_call": True,
            "unsaved_context_draft_retained": True,
            "image_version_record": image_record["sequence"],
            "original_and_sent_raster_preview": True,
            "main_transcript_original_image_pixels": True,
            "reference_catalog_search": True,
            "late_source_preview_preserves_current_search": True,
            "reference_catalog_branch_preview": True,
            "reference_images_explicit": True,
            "source_growth_does_not_change_reference": True,
            "frozen_reference_recovered_and_sent": True,
            "narrow_resize": True,
            "terminal_modes_restored": True,
            "controlled_provider_requests": len(server.requests),
            "native_graphics": "not exercised by POSIX PTY; raster fallback verified",
        }
    finally:
        if gate:
            gate.release.set()
            gate.preview_release.set()
        try:
            if terminal and not terminal.closed:
                for _ in range(3):
                    if terminal.process.poll() is None:
                        terminal.send(b"\x1b")
                terminal.close()
        finally:
            if host and host.poll() is None:
                try:
                    if endpoint:
                        call(endpoint, "/shutdown", {})
                    assert host.wait(timeout=15) == 0
                finally:
                    if host.poll() is None:
                        host.kill()
                        host.wait()
            if gate:
                gate.close()
            server.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--composition", type=Path)
    parser.add_argument("--label", default="candidate")
    args = parser.parse_args()
    receipt = None if args.binary else ROOT / "artifacts/shared-context-tui-verification.json"
    if receipt:
        receipt.unlink(missing_ok=True)
    result: dict[str, Any] = {"platform": sys.platform, "label": args.label}
    if os.name == "nt":
        result.update(
            status="skipped",
            pty="skipped",
            reason="POSIX PTY unavailable; native Windows terminal evidence required",
        )
    else:
        if args.binary:
            if not args.composition:
                parser.error("--binary requires --composition")
            binary, composition = args.binary.resolve(), args.composition.resolve()
            result["installation"] = "explicit existing bytes"
        else:
            prepare()
            destination = installed(ROOT / "artifacts/shared-context-tui-install")
            binary, composition = destination / "bin/eden", destination / "composition.json"
            result["installation"] = "shared frozen verification preparation"
        with tempfile.TemporaryDirectory(prefix="eden-shared-context-tui-") as directory:
            result.update(exercise(binary, composition, Path(directory)))
        result.update(status="passed", pty="passed")
    if receipt:
        receipt.parent.mkdir(parents=True, exist_ok=True)
        receipt.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
