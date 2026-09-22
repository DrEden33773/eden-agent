#!/usr/bin/env python3
"""Installed S7: selected export, fixed publication, external roles and complete updates."""

import copy
import http.server
import json
import os
import pathlib
import platform
import signal
import socket
import subprocess
import tempfile
import threading
import time
from typing import Any

from delivery_updates import verify_updates
from http_fixture import FixtureHTTPServer
from install import ROOT, package, target
from verification import author_artifact, example, installed, run


def write(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value) + "\n", encoding="utf-8")


def main() -> None:
    results: dict[str, Any] = {
        "platform": platform.platform(),
        "real_gist": "not authorized",
        "real_release": "not published",
    }
    with tempfile.TemporaryDirectory(prefix="eden-delivery-") as temporary:
        scratch = pathlib.Path(temporary)
        installation = installed(scratch / "install")
        host = installation / "bin" / ("eden.exe" if os.name == "nt" else "eden")
        config = installation / "composition.json"
        selected = json.loads(config.read_text(encoding="utf-8"))
        environment = {
            **os.environ,
            "EDEN_AGENT_DIR": str(scratch / "global"),
            "GH_TOKEN": "controlled-gist-token",
        }

        def invoke(arguments: list[str | pathlib.Path], success: bool = True) -> Any:
            result = run(
                [host, "--json", "--offline-startup", "--composition", config, *arguments],
                scratch,
                check=False,
                env=environment,
            )
            if success:
                assert result.returncode == 0, (result.stdout, result.stderr)
                return json.loads(result.stdout)
            assert result.returncode != 0, result.stdout
            return result

        history = scratch / "history.jsonl"
        records = [
            {
                "schema_version": 2,
                "session_id": 1,
                "sequence": 1,
                "parent_id": None,
                "branch": "main",
                "run_id": 0,
                "kind": "composition_lock",
                "payload": {"secret": "CONFIG_CANARY"},
            },
            {
                "schema_version": 2,
                "session_id": 1,
                "sequence": 2,
                "parent_id": 1,
                "branch": "main",
                "run_id": 1,
                "kind": "message",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "text", "text": "Hello 中文 <script>bad()</script>"}],
                },
            },
        ]
        write(history, {"schema_version": 2, "transaction": records})
        # The default exporter must succeed with every business library absent.
        stripped = copy.deepcopy(selected)
        for item in stripped["packages"]:
            if item["descriptor"]["package"] not in {
                "local-export",
                "github-share",
                "distribution",
            }:
                item["library"] = "missing-original-plugin"
        write(config, stripped)
        preview = scratch / "preview.html"
        reply = invoke(["export", history, preview])
        frozen = preview.read_text(encoding="utf-8")
        assert "CONFIG_CANARY" not in frozen and "&lt;script&gt;" in frozen and "中文" in frozen
        backup = scratch / "backup.jsonl"
        copied = run([host, "history", "export", history, backup], scratch, env=environment)
        assert copied.returncode == 0 and "CONFIG_CANARY" in backup.read_text(encoding="utf-8")
        requests: list[dict[str, Any]] = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                assert self.headers["Authorization"] == "Bearer controlled-gist-token"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                if len(requests) == 2:
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                self.send_response(201)
                self.end_headers()
                self.wfile.write(b'{"html_url":"https://gist.github.com/controlled"}')

        server = FixtureHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            for item in stripped["packages"]:
                if item["descriptor"]["package"] == "github-share":
                    item["config"] = {
                        "endpoint": f"http://127.0.0.1:{server.server_address[1]}/gists"
                    }
            write(config, stripped)
            invoke(["share", preview, "--sha256", reply["sha256"]], False)
            assert not requests
            history.write_text("conversation changed after preview", encoding="utf-8")
            published = invoke(["share", preview, "--sha256", reply["sha256"], "--confirm"])
            assert published["visibility"] == "secret" and "Anyone with" in published["notice"]
            assert (
                requests[0]["public"] is False
                and requests[0]["files"]["conversation.html"]["content"] == frozen
            )
            lost = invoke(["share", preview, "--sha256", reply["sha256"], "--confirm"], False)
            assert "PublicationUnknown" in lost.stderr and len(requests) == 2
            preview.write_text(frozen + "edited", encoding="utf-8")
            invoke(["share", preview, "--sha256", reply["sha256"], "--confirm"], False)
            assert len(requests) == 2
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        results["export_without_business_plugins_and_fixed_gist_bytes"] = True
        write(history, {"schema_version": 2, "transaction": records})
        author = author_artifact("delivery-services")
        roles = ["eden.exporter.v1", "eden.share-target.v1", "eden.update-source.v1"]
        external = {
            "packages": [
                package(
                    "delivery-services", [*roles, "eden.instance-stop.v1"], str(author), target()
                )
            ],
            "roles": {role: "delivery-services" for role in roles},
        }
        write(config, external)
        custom = scratch / "custom.html"
        custom_reply = invoke(["export", history, custom])
        assert custom.read_text(encoding="utf-8") == "External renderer: 2 records"
        custom_share = invoke(["share", custom, "--sha256", custom_reply["sha256"], "--confirm"])
        assert custom_share["url"] == "https://example.invalid/author-target" and custom_share[
            "content"
        ] == custom.read_text(encoding="utf-8")
        assert invoke(["update"])["targets"][0]["current_version"] == "external-source"
        if os.name != "nt":
            marker = scratch / "export.ready"
            external["packages"][0]["config"] = {"marker": str(marker)}
            write(config, external)
            cancelled_preview = scratch / "cancelled.html"
            pending = subprocess.Popen(
                [
                    str(host),
                    "--composition",
                    str(config),
                    "export",
                    str(history),
                    str(cancelled_preview),
                ],
                cwd=scratch,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while (
                    not marker.exists() and pending.poll() is None and time.monotonic() < deadline
                ):
                    time.sleep(0.02)
                assert marker.exists(), "exporter did not become ready"
                pending.send_signal(signal.SIGINT)
                _, error = pending.communicate(timeout=10)
                assert pending.returncode != 0 and "Cancelled" in error
                assert marker.with_suffix(".cleaned").exists() and not cancelled_preview.exists()
            finally:
                if pending.poll() is None:
                    pending.kill()
                    pending.wait()
        external["packages"][0]["config"] = {"fail": True}
        write(config, external)
        failed = invoke(["export", history, scratch / "failed.html"], False)
        assert "export failed" in failed.stderr and "finalizer failed" in failed.stderr
        results["three_external_sdk_roles"] = True
        write(config, selected)
        sdk = run([example("delivery_probe"), config], scratch, env=environment)
        assert sdk.returncode == 0, (sdk.stdout, sdk.stderr)
        selected["packages"].append(
            package(
                "delivery-services",
                [*roles, "eden.instance-stop.v1"],
                str(author),
                target(),
                {"bytes": 2 * 1024 * 1024},
            )
        )
        selected["roles"]["eden.exporter.v1"] = "delivery-services"
        selected["roles"]["eden.share-target.v1"] = "delivery-services"
        write(config, selected)
        process = subprocess.Popen(
            [str(host), "--offline-startup", "--composition", str(config), "rpc"],
            cwd=scratch,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        assert process.stdin is not None and process.stdout is not None

        while True:
            frame = json.loads(process.stdout.readline())
            if frame.get("type") == "ready":
                session_id = frame["session_id"]
                break

        def request(identity: str, method: str, params: dict[str, Any]) -> dict[str, Any]:
            assert process.stdin is not None and process.stdout is not None
            process.stdin.write(
                json.dumps(
                    {
                        "version": 1,
                        "id": identity,
                        "session_id": session_id,
                        "method": method,
                        "params": params,
                    }
                )
                + "\n"
            )
            process.stdin.flush()
            while True:
                line = process.stdout.readline()
                assert line, "RPC exited without response"
                value = json.loads(line)
                if value.get("id") == identity:
                    assert value["type"] == "result", value
                    return value["result"]

        try:
            exported = request("export", "export", {})
            assert len(exported["content"]) == 2 * 1024 * 1024
            process.stdin.write(
                json.dumps(
                    {
                        "version": 1,
                        "id": "publish",
                        "session_id": session_id,
                        "method": "share",
                        "params": {"preview_id": exported["preview_id"], "confirmed": True},
                    }
                )
                + "\n"
            )
            process.stdin.flush()
            while True:
                value = json.loads(process.stdout.readline())
                if (
                    value.get("id") == "publish"
                    and value.get("type") == "event"
                    and value["event"]["kind"] == "settled"
                ):
                    assert (
                        len(value["event"]["payload"]["outcome"]["value"]["content"])
                        == 2 * 1024 * 1024
                    ), value
                    break
            assert request("forget", "preview.forget", {"preview_id": exported["preview_id"]})[
                "forgotten"
            ]
            request("close", "shutdown", {})
            assert process.wait(timeout=20) == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            process.stdin.close()
            process.stdout.close()
            if process.stderr is not None:
                process.stderr.close()
        assert not list((scratch / "global").rglob("*.jsonl"))
        results["sdk_rpc_memory_export"] = True
        selected["packages"] = [
            p for p in selected["packages"] if p["descriptor"]["package"] != "delivery-services"
        ]
        selected["roles"]["eden.exporter.v1"] = "local-export"
        selected["roles"]["eden.share-target.v1"] = "github-share"
        write(config, selected)
        results["updates"] = verify_updates(installation, scratch / "updates")
    write(ROOT / "artifacts/delivery-verification.json", results)
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
