#!/usr/bin/env python3
"""Exercise one external native presentation author through a separate live host process."""

import json
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

from install import ROLES, ROOT, package, target
from verification import author_artifact, installed, prepare

ACTION = "eden.presentation.action.v1"
PROVIDES = [ROLES[0], ACTION, *ROLES[1:]]


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


def main() -> None:
    prepare()
    destination = installed(ROOT / "artifacts/install-presentation", controlled=True)
    with tempfile.TemporaryDirectory(prefix="eden-presentation-") as temp:
        scratch = pathlib.Path(temp)
        binary = destination / "bin" / ("eden.exe" if sys.platform == "win32" else "eden")
        native = author_artifact("presentation-live")
        composition = {
            "packages": [package("presentation-live", PROVIDES, str(native), target())],
            "roles": {role: "presentation-live" for role in ROLES},
            "resource_packages": [],
        }
        composition_file = scratch / "composition.json"
        composition_file.write_text(json.dumps(composition), encoding="utf-8")
        endpoint_file = scratch / "endpoint.json"
        command = [
            binary,
            "--composition",
            composition_file,
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
            call(endpoint, "/shutdown", {})
            assert host.wait(timeout=15) == 0
            assert not endpoint_file.exists()
            evidence = {
                "session_id": endpoint["session_id"],
                "view_revision": view["revision"],
                "action_calls": result["calls"],
                "cancelled_run": run,
                "late_dialog_run": late,
                "host_exit": 0,
            }
            artifact = ROOT / "artifacts/presentation-verification.json"
            artifact.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
            print(json.dumps(evidence))
        finally:
            if host.poll() is None:
                host.kill()
                host.wait()
            if host.returncode and host.stderr is not None:
                print(host.stderr.read(), file=sys.stderr)


if __name__ == "__main__":
    main()
