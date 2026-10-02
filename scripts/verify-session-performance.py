#!/usr/bin/env python3
"""Measure installed Session stages in isolated copies using causal frames and real shell exit."""

import argparse
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path.cwd() / "scripts"))
from session_fixture import Terminal, ready_view, streaming, trace_rows, wait_until

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--installation", type=Path, required=True)
p.add_argument("--sessions", type=Path, required=True)
p.add_argument("--small-history", required=True)
p.add_argument("--large-history", required=True)
p.add_argument("--quit-key", choices=["c", "d"], default="d")
p.add_argument("--assert-budgets", action="store_true")
p.add_argument("--shape", choices=["empty", "latest", "all"], required=True)
p.add_argument("--open", choices=["none", "latest", "old"], default="none")
p.add_argument("--probe-services", action="store_true")
p.add_argument("--prehost-latest", action="store_true")
p.add_argument("--compatible-latest", action="store_true")
p.add_argument("--trim-audit", action="store_true")
p.add_argument("--manage", action="store_true")
p.add_argument("--output", required=True)
a = p.parse_args()
install = a.installation.resolve()
candidate = json.loads((install / "CANDIDATE.json").read_text())
source = a.sessions.resolve()
out = Path(a.output)
out.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="eden-perf-") as tmp:
    root = Path(tmp)
    dest = root / ".eden/sessions"
    dest.mkdir(parents=True)
    state = root / "state"
    trace = root / "wire.jsonl"
    names = (
        [a.small_history]
        if a.shape == "latest"
        else [x.name for x in source.glob("*.jsonl")]
        if a.shape == "all"
        else []
    )
    for name in names:
        shutil.copy2(source / name, dest / name)
    if a.shape == "all" and (source / ".trash").exists():
        shutil.copytree(source / ".trash", dest / ".trash")
    for path in dest.rglob("*.jsonl"):
        with path.open("rb") as original:
            first = json.loads(original.readline())
            rest = original.read()
        for record in first.get("transaction", []):
            if record.get("kind") == "session":
                record["payload"]["cwd"] = str(root)
            if (
                a.compatible_latest
                and path.name == a.small_history
                and record.get("kind") == "composition_lock"
            ):
                record["payload"]["cwd"] = str(root)
        path.write_bytes(json.dumps(first).encode() + b"\n" + rest)
    for path in (dest / ".trash").glob("*/entry.json"):
        entry = json.loads(path.read_text())
        entry["original"] = str(dest / Path(entry["original"]).name)
        entry["directory"] = str(path.parent)
        entry["cwd"] = str(root)
        path.write_text(json.dumps(entry))
    config = json.loads((install / "composition.json").read_text())
    for package in config["packages"]:
        package["library"] = str((install / package["library"]).resolve())
        if package["descriptor"]["package"] == "model-access":
            package["config"] = {
                "catalog": {"offline": True, "models": []},
                "credentials": {
                    "path": str(root / "global/credentials.json"),
                    "providers": {
                        provider: {"cloud": {"enabled": False}}
                        for provider in ("amazon-bedrock", "google-vertex")
                    },
                },
            }
    if a.trim_audit:
        for path in dest.glob("*.jsonl"):
            target = path.with_suffix(".projected")
            with path.open() as original, target.open("w") as output:
                for line in original:
                    envelope = json.loads(line)
                    for record in envelope.get("transaction", []):
                        if record["kind"] in [
                            "model_request",
                            "model_request_revision",
                        ] and isinstance(record["payload"], dict):
                            record["payload"].pop("input", None)
                    output.write(
                        json.dumps(envelope, separators=(",", ":"), ensure_ascii=False) + "\n"
                    )
            target.replace(path)
    if a.compatible_latest:
        small = dest / a.small_history
        lines = small.read_bytes().splitlines(keepends=True)
        updated = []
        for line in lines:
            envelope = json.loads(line)
            for record in envelope.get("transaction", []):
                if record["kind"] == "composition_lock":
                    lock = record["payload"]
                    lock["cwd"] = str(root)
                    lock["roles"] = {
                        role: package
                        for role, package in config["roles"].items()
                        if role not in ("eden.interaction.v1", "eden.presentation.host.v1")
                    }
                    for package in config["packages"]:
                        name = package["descriptor"]["package"]
                        if name in lock.get("packages", {}):
                            lock["packages"][name]["sha256"] = hashlib.sha256(
                                Path(package["library"]).read_bytes()
                            ).hexdigest()
                            lock["packages"][name]["descriptor"] = package["descriptor"]
                    lock["library_locations"] = [
                        package["library"] for package in config["packages"]
                    ]
            updated.append(
                json.dumps(envelope, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"
            )
        small.write_bytes(b"".join(updated))
    composition = root / "composition.json"
    composition.write_text(json.dumps(config))
    rc = root / "bash.rc"
    rc.write_text("PS1='PERF_SHELL> '\nPROMPT_COMMAND=\n")
    fixture_env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(
            (
                "AWS_",
                "OPENAI_",
                "ANTHROPIC_",
                "GOOGLE_",
                "AZURE_",
                "CLOUDFLARE_",
                "DEEPSEEK_",
                "XAI_",
                "GROK_",
                "COPILOT_",
                "RADIUS_",
            )
        )
        and not key.endswith(("_API_KEY", "_TOKEN"))
    }
    fixture_env.update(
        {
            "AWS_CONFIG_FILE": str(root / "aws-config"),
            "AWS_SHARED_CREDENTIALS_FILE": str(root / "aws-credentials"),
            "AWS_EC2_METADATA_DISABLED": "true",
            "HISTFILE": str(root / "shell-history"),
        }
    )
    prehost = None
    pre_endpoint = root / "prehost.json"
    if a.prehost_latest:
        prehost = subprocess.Popen(
            [
                str(install / "bin/eden"),
                "--composition",
                str(composition),
                "--global-dir",
                str(root / "global"),
                "--no-trust-project",
                "--offline-startup",
                "--session",
                str(dest / a.small_history),
                "resume-live",
                "--endpoint",
                str(pre_endpoint),
            ],
            stdout=subprocess.DEVNULL,
            stderr=(out / "prehost-private.log").open("wb"),
            env={**fixture_env, "EDEN_TUI_STATE_DIR": str(state)},
        )
        wait_until(
            lambda prehost=prehost: pre_endpoint.exists() or prehost.poll() is not None, seconds=30
        )
        assert pre_endpoint.exists(), "isolated prehost failed; private diagnostics retained"
    terminal = Terminal(
        install / "bin/eden",
        root / "endpoint.json",
        width=110,
        height=40,
        command=[
            "script",
            "--quiet",
            "--return",
            "--command",
            f"bash --noprofile --rcfile {shlex.quote(str(rc))} -i",
            "/dev/null",
        ],
        inherit_env=False,
        env={
            **fixture_env,
            "EDEN_TUI_STATE_DIR": str(state),
            "EDEN_FRONTEND_TRACE": str(trace),
            "EDEN_LEGACY_SESSION_DIRS": "",
            "EDEN_LATENCY_TRACE": str(out / "latency.jsonl"),
        },
    )
    result = {
        "prehost_latest": a.prehost_latest,
        "compatible_latest": a.compatible_latest,
        "trim_audit": a.trim_audit,
        "manage": a.manage,
        "shape": a.shape,
        "open": a.open,
        "installation_commit": candidate["commit"],
        "source_fingerprint": candidate["source_fingerprint"],
        "quit_key": a.quit_key,
        "history_count": len(names),
        "copied_bytes": sum((dest / x).stat().st_size for x in names),
    }
    try:
        wait_until(lambda: b"PERF_SHELL> " in terminal.output, terminal)
        command = [
            str(install / "bin/eden"),
            "--composition",
            str(composition),
            "--global-dir",
            str(root / "global"),
            "--no-trust-project",
            "--offline-startup",
            "--cwd",
            str(root),
        ]
        started = time.monotonic()
        terminal.command(shlex.join(command))
        wait_until(trace.exists, terminal, seconds=120)
        ready_view(terminal, trace, 0)
        result["start_to_ready_seconds"] = time.monotonic() - started
        print(json.dumps({"stage": "startup", **result}), flush=True)
        if a.probe_services:
            ep = json.loads(next(state.glob("hosts/*.json")).read_text())
            pid = ep["pid"]
            result["service_probes"] = []

            def cpu():
                fields = Path(f"/proc/{pid}/stat").read_text().split(")", 1)[1].split()
                return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")

            for route in [
                "/snapshot",
                "/models/list",
                "/models/current",
                "/resources",
                "/context/inspect",
            ]:
                for repeat in range(3):
                    before = cpu()
                    started = time.monotonic()
                    fault = None
                    try:
                        value = streaming.live.call(ep, route, None if route == "/snapshot" else {})
                    except RuntimeError as error:
                        value = {}
                        fault = type(error).__name__
                    elapsed = time.monotonic() - started
                    used = cpu() - before
                    row = {
                        "route": route,
                        "repeat": repeat,
                        "fault": fault,
                        "seconds": elapsed,
                        "host_cpu_seconds": used,
                        "wire_bytes": len(json.dumps(value, separators=(",", ":")).encode()),
                        "model_count": len(value.get("models", []))
                        if route == "/models/list"
                        else None,
                    }
                    result["service_probes"].append(row)
                    print(json.dumps({"service_probe": row}), flush=True)
        if a.open != "none" or a.manage:
            result["stage"] = "directory"
            started = time.monotonic()
            offset = len(trace_rows(trace))
            terminal.command("/resume")
            wait_until(
                lambda: any(
                    r.get("direction") == "frame"
                    and r.get("method") == "directory/accepted"
                    and r.get("count", 0) > 0
                    for r in trace_rows(trace)[offset:]
                ),
                terminal,
                seconds=120,
            )
            result["directory_first_seconds"] = time.monotonic() - started
            wait_until(
                lambda: any(
                    r.get("phase") == "directory" and r.get("edge") == "end"
                    for r in trace_rows(trace)[offset:]
                ),
                terminal,
                seconds=120,
            )
            completed = next(
                r
                for r in reversed(trace_rows(trace)[offset:])
                if r.get("phase") == "directory" and r.get("edge") == "end"
            )
            wait_until(
                lambda: any(
                    r.get("direction") == "frame"
                    and r.get("method") == "directory/accepted"
                    and r.get("time_ns", 0) >= completed["time_ns"]
                    for r in trace_rows(trace)[offset:]
                ),
                terminal,
                seconds=120,
            )
            terminal.drain()
            result["directory_seconds"] = time.monotonic() - started
            result["stage"] = "selection"
            name = a.small_history if a.open == "latest" else a.large_history
            selected_bytes = (dest / name).read_bytes()
            started = time.monotonic()
            offset = len(trace_rows(trace))
            terminal.send(b"/" + Path(name).stem.encode())
            terminal.wait(Path(name).stem[:18], seconds=30)
            if a.manage:
                original_bytes = (dest / name).read_bytes()
                terminal.send(b"\x1b[B")
                result["stage"] = "trash-preview"
                previewed = time.monotonic()
                terminal.send(b"d")
                terminal.wait("Move session to Trash?", seconds=120)
                result["trash_preview_seconds"] = time.monotonic() - previewed

                def click(label):
                    terminal.wait(label, seconds=120)
                    lines = terminal.display.splitlines()
                    row = next(i for i, line in enumerate(lines) if f"[ {label} ]" in line)
                    column = lines[row].index(label) + 2
                    terminal.send(
                        f"\x1b[<0;{column};{row + 1}M\x1b[<0;{column};{row + 1}m".encode()
                    )

                result["stage"] = "trash-apply"
                applied = time.monotonic()
                click("Move to Trash")
                wait_until(lambda: not (dest / name).exists(), terminal, seconds=120)
                result["trash_apply_seconds"] = time.monotonic() - applied
                wait_until(
                    lambda: "Move session to Trash?" not in terminal.display, terminal, seconds=120
                )
                result["trash_to_operable_ui_seconds"] = time.monotonic() - applied
                terminal.wait("Resume session", seconds=120)
                terminal.send(b"\x1b")
                terminal.send(b"\x1b")
                wait_until(lambda: "Resume session" not in terminal.display, terminal)
                result["stage"] = "restore-picker"
                terminal.command("/resume")
                terminal.wait("Resume session", seconds=120)
                terminal.send(b"f")
                terminal.wait("All saved")
                terminal.send(b"f")
                terminal.wait("Trash")
                entry = next(
                    json.loads(p.read_text())
                    for p in (dest / ".trash").glob("*/entry.json")
                    if json.loads(p.read_text())["original"] == str(dest / name)
                )
                terminal.send(b"/" + entry["title"].encode())
                terminal.wait(entry["title"][:18])
                terminal.send(b"\r")
                terminal.wait("Restore session", seconds=120)
                result["stage"] = "restore-apply"
                restored = time.monotonic()
                click("Restore session")
                wait_until((dest / name).exists, terminal, seconds=120)
                result["restore_seconds"] = time.monotonic() - restored
                result["restore_preserved_exact_bytes"] = (
                    dest / name
                ).read_bytes() == original_bytes
                assert result["restore_preserved_exact_bytes"]
                wait_until(lambda: "Restore session" not in terminal.display, terminal, seconds=120)
                terminal.send(b"\x1b")
                terminal.send(b"\x1b")
                wait_until(lambda: "Resume session" not in terminal.display, terminal)
            else:
                terminal.send(b"\r")
                result["stage"] = "loading"
                ready_view(terminal, trace, offset)
                result["history_to_ready_seconds"] = time.monotonic() - started
                result["open_preserved_exact_bytes"] = (dest / name).read_bytes() == selected_bytes
                assert result["open_preserved_exact_bytes"], "opening mutated saved history"
                if a.compatible_latest:
                    endpoints = [*state.glob("hosts/*.json")]
                    if prehost:
                        endpoints.append(pre_endpoint)
                    states = [
                        streaming.live.call(json.loads(endpoint.read_text()), "/snapshot")
                        for endpoint in endpoints
                    ]
                    selected = next(
                        value
                        for value in states
                        if value.get("history_path") == str((dest / name).resolve())
                    )
                    result["current_binding_execution_view"] = (
                        selected["state"].get("read_only", False) is False
                        and selected["state"]["closed"] is False
                    )
                    assert result["current_binding_execution_view"], (
                        "current fixture fell back to a reader"
                    )
            print(json.dumps({"stage": "opened", **result}), flush=True)
        terminal.send(b"\x03" if a.quit_key == "c" else b"\x04")
        started = time.monotonic()
        offset = len(terminal.output)
        terminal.send(b"\x03" if a.quit_key == "c" else b"\x04")
        wait_until(lambda: b"PERF_SHELL> " in terminal.output[offset:], terminal, seconds=120)
        result["second_quit_to_prompt_seconds"] = time.monotonic() - started
        marker = f"PERF_{time.monotonic_ns()}_READY"
        x, y, z = marker.split("_")
        terminal.command(f"printf '%s_%s_%s\\n' {x} {y} {z}")
        wait_until(lambda: (marker + "\r\n").encode() in terminal.output, terminal)
        result["second_quit_to_command_seconds"] = time.monotonic() - started
        if prehost:
            assert prehost.poll() is None, "detach stopped the borrowed owner"
            result["borrowed_owner_retained"] = True
        terminal.command("exit")
        wait_until(lambda: terminal.process.poll() is not None, terminal)
        terminal.close(screen=False)
        budgets = {
            "start_to_ready_seconds": 2.75,
            "history_to_ready_seconds": 4.0 if a.open == "old" else 2.5,
            "second_quit_to_command_seconds": 1.0,
            "directory_seconds": 0.75,
            "trash_preview_seconds": 1.25,
            "trash_to_operable_ui_seconds": 1.25,
            "restore_seconds": 1.25,
        }
        result["exploratory_two_second_targets"] = [
            key
            for key in ("start_to_ready_seconds", "history_to_ready_seconds")
            if result.get(key, 0) > 2.0
        ]
        result["budgets"] = budgets
        result["red_symptoms"] = [k for k, v in budgets.items() if result.get(k, 0) > v]
    except Exception as e:
        (out / "failure-private.txt").write_text(terminal.display)
        result["error_type"] = type(e).__name__
        result["red_symptoms"] = ["causal_frame_or_shell_timeout"]
        print(
            json.dumps(
                {
                    "stage": "failure",
                    "shape": a.shape,
                    "open": a.open,
                    "error_type": type(e).__name__,
                }
            ),
            flush=True,
        )
    finally:
        if not terminal.closed:
            try:
                terminal.close(screen=False)
            except Exception:
                pass
        for ep in state.glob("hosts/*.json"):
            try:
                metadata = json.loads(ep.read_text())
                if "address" in metadata:
                    streaming.live.call(
                        metadata, "/shutdown", {"session_id": metadata["session_id"]}
                    )
            except (OSError, RuntimeError):
                pass
        if prehost:
            try:
                prehost.wait(timeout=15)
            except subprocess.TimeoutExpired:
                prehost.kill()
                prehost.wait()
        if trace.exists():
            shutil.copy2(trace, out / "wire-private.jsonl")
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({"record_type": "final", **result}), flush=True)

if result.get("error_type") or (a.assert_budgets and result["red_symptoms"]):
    raise SystemExit(1)
