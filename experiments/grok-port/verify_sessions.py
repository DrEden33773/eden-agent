#!/usr/bin/env python3
"""Session discovery and failed rename regressions against the installed native host."""

import argparse
import json
import os
import shutil
import subprocess
from pathlib import Path
from unittest.mock import patch

import launch
import verify as probe
from verify_workflows import ROOT, Fixture, Rpc


def launches(fixture):
    project = fixture.root / "project with spaces"
    project.mkdir()
    alias = fixture.root / "alias"
    alias.symlink_to(project, target_is_directory=True)
    hosts = []
    endpoints = []
    saved = []
    original_popen = subprocess.Popen

    def start(command, **kwargs):
        if "--endpoint" not in command:
            return original_popen(command, **kwargs)
        command = [
            command[0],
            "--composition",
            str(fixture.composition),
            "--global-dir",
            str(fixture.root / "global"),
            *command[1:],
        ]
        kwargs["env"] = {**os.environ, "EDEN_TUI_STATE_DIR": str(fixture.root / "host-state")}
        host = original_popen(command, **kwargs)
        hosts.append(host)
        endpoints.append(Path(command[command.index("--endpoint") + 1]))
        return host

    def frontend(command, **kwargs):
        endpoint = endpoints[-1]
        metadata = json.loads(endpoint.read_text())
        fixture.children.append(metadata)
        rpc = Rpc(endpoint)
        try:
            identity = f"eden-{metadata['session_id']}"
            rpc.call("session/load", {"sessionId": identity})
            if not saved:
                rpc.call(
                    "_x.ai/session/rename", {"sessionId": identity, "title": "Across launchers"}
                )
                rows = rpc.call("_x.ai/session/list", {})["sessions"]
                row = next(r for r in rows if r["summary"] == "Across launchers")
                saved.append(row["sessionId"])
                assert Path(saved[0].removeprefix("history:")).parent == project / ".eden/sessions"
            else:
                rows = rpc.call("_x.ai/session/list", {})["sessions"]
                assert any(
                    r["sessionId"] == saved[0] and r["summary"] == "Across launchers" for r in rows
                ), rows
                rpc.call("session/load", {"sessionId": saved[0]})
                rpc.call("_x.ai/session/rename", {"sessionId": saved[0], "title": "Resumed name"})
                rows = rpc.call("_x.ai/session/list", {})["sessions"]
                assert any(r["summary"] == "Resumed name" for r in rows)
        finally:
            rpc.close()
            probe.streaming.live.call(metadata, "/shutdown", {})
        return 0

    for cwd in (project, alias):
        with (
            patch.object(launch, "ROOT", fixture.root / "launcher"),
            patch(
                "sys.argv",
                ["launch.py", "--cwd", str(cwd), "--host", str(fixture.installation / "bin/eden")],
            ),
            patch.object(launch.subprocess, "Popen", side_effect=start),
            patch.object(launch.subprocess, "call", side_effect=frontend),
        ):
            try:
                launch.main()
            except SystemExit as result:
                assert result.code == 0
        hosts[-1].wait(timeout=15)
    fixture.checks["real_launcher_close_fresh_discovery_resume_canonical_cwd"] = True
    legacy = fixture.root / "legacy"
    (legacy / "same").mkdir(parents=True)
    (legacy / "other").mkdir()
    same = legacy / "same/history.jsonl"
    other = legacy / "other/history.jsonl"
    shutil.copyfile(fixture.history, same)
    shutil.copyfile(saved[0].removeprefix("history:"), other)
    with patch.dict(os.environ, {"EDEN_GROK_LEGACY_SESSIONS": str(legacy)}):
        rpc = Rpc(fixture.endpoint_file)
    try:
        rows = rpc.call("_x.ai/session/list", {})["sessions"]
        assert any(r["sessionId"] == f"history:{same}" for r in rows), rows
        assert not any(r["sessionId"] == f"history:{other}" for r in rows), rows
    finally:
        rpc.close()
    fixture.checks["legacy_uuid_discovery_filters_recorded_cwd"] = True


def management(fixture):
    page = fixture.form("sessions")
    opened = fixture.action(page, "new")
    identity = opened["loadSession"]
    assert identity != fixture.identity
    assert fixture.rpc is not None
    fixture.rpc.call("session/load", {"sessionId": identity})
    fixture.rpc.call("_x.ai/session/rename", {"sessionId": identity, "title": "Managed new"})
    rows = fixture.rpc.call("_x.ai/session/list", {})["sessions"]
    source = next(r["sessionId"] for r in rows if r["summary"] == "Managed new")
    page = fixture.form("sessions")
    selected = fixture.action(page, source)
    tree = fixture.action(selected, "tree")
    target = tree["actions"][-1]["id"]
    navigation = fixture.action(tree, target)
    result = fixture.action(
        navigation, "navigate", inputs={"branch": "test-branch", "summarize": False}
    )
    fixture.rpc.call("session/load", {"sessionId": result["loadSession"]})
    records = fixture.call("/manage/tree", {"path": source.removeprefix("history:")})
    assert any(r["branch"] == "test-branch" for r in records), records[-1]
    fixture.checks["new_tree_branch_durable"] = True
    for kind in ("clone", "fork"):
        page = fixture.form("sessions")
        selected = fixture.action(page, source)
        copy = fixture.action(selected, kind)
        if kind == "fork":
            copy = fixture.action(copy, target)
        destination = fixture.root / f"{kind}.jsonl"
        preview = fixture.action(copy, "preview", inputs={"destination": str(destination)})
        assert not destination.exists()
        result = fixture.action(preview, "apply")
        assert destination.exists()
        fixture.rpc.call("session/load", {"sessionId": result["loadSession"]})
        fixture.checks[f"{kind}_preview_apply_open"] = True
        if kind == "clone":
            page = fixture.form("sessions")
            selected = fixture.action(page, f"history:{destination}")
            confirmation = fixture.action(selected, "delete")
            try:
                fixture.action(confirmation, "delete", inputs={"confirmed": True})
            except RuntimeError as error:
                assert "active" in str(error), error
            else:
                raise AssertionError("active writer was deleted")
            info = fixture.call("/manage/info", {"path": str(destination)})
            endpoint = json.loads(Path(info["owner"]).read_text())
            probe.streaming.live.call(endpoint, "/shutdown", {})
            # The shutdown response acknowledges intent; wait on registry removal before delete.
            probe.streaming.live.wait_for(
                fixture.endpoint, lambda _, owner=Path(info["owner"]): not owner.exists()
            )
            fixture.action(confirmation, "delete", inputs={"confirmed": True})
            assert not destination.exists()
            assert Path(source.removeprefix("history:")).exists()
            fixture.checks["delete_checks_live_writer_and_preserves_source"] = True


def stop_resume(fixture):
    assert fixture.rpc is not None
    page = fixture.form("sessions")
    selected = fixture.action(page, f"history:{fixture.history}")
    opened = fixture.action(selected, "open")
    identity = opened["loadSession"]
    fixture.rpc.call("session/load", {"sessionId": identity})
    page = fixture.form("sessions")
    selected = fixture.action(page, f"history:{fixture.history}")
    confirmation = fixture.action(selected, "stop")
    fixture.action(confirmation, "stop", inputs={"confirmed": True})
    fixture.host.wait(timeout=15)
    page = fixture.form("sessions")
    selected = fixture.action(page, f"history:{fixture.history}")
    opened = fixture.action(selected, "open")
    fixture.rpc.call("session/load", {"sessionId": opened["loadSession"]})
    fixture.rpc.call(
        "_x.ai/session/rename", {"sessionId": opened["loadSession"], "title": "After root stop"}
    )
    rows = fixture.rpc.call("_x.ai/session/list", {})["sessions"]
    assert any(r["summary"] == "After root stop" for r in rows)
    fixture.checks["stop_root_handoff_resume_same_frontend"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/s4-g2-session-host")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output, configuration_author=False) as fixture:
        assert fixture.rpc is not None
        fixture.rpc.call(
            "_x.ai/session/rename", {"sessionId": fixture.identity, "title": "Original title"}
        )
        fixture.terminal_start()
        assert fixture.terminal is not None
        fixture.terminal.command("/sessions")
        fixture.click("Original title")
        fixture.click("Read-only")
        fixture.terminal.wait("Read-only history.")
        fixture.terminal.command("/rename REJECTED_TITLE")
        fixture.terminal.wait("Couldn't rename session")
        fixture.capture("readonly-rename")
        screen = (args.output / "readonly-rename.txt").read_text()
        assert "REJECTED_TITLE" not in screen, "failed rename left a false title"
        fixture.checks["failed_rename_keeps_committed_title"] = True
        fixture.terminal.command("/sessions")
        fixture.click("New Session in this cwd")
        fixture.terminal.wait("Session opened")
        fixture.terminal.command("/rename From readonly")
        fixture.terminal.wait('Session renamed to "From readonly"')
        assert any(
            row["name"] == "From readonly"
            for row in fixture.call(
                "/manage/sessions", {"directory": str(fixture.root / ".eden/sessions")}
            )
        )
        fixture.checks["new_from_readonly_uses_management_host_and_saved_cwd"] = True
        fixture.stop_terminal()
        launches(fixture)
        management(fixture)
        stop_resume(fixture)


if __name__ == "__main__":
    main()
