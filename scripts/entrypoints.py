"""Cross-entrypoint semantic checks using the installed product and a bounded RPC reader."""

import json
import os
import pathlib
import queue
import shutil
import subprocess
import threading
from typing import Any

from install import library, package, target
from verification import ROOT, author_artifact, installed, run


def check(host: pathlib.Path, composition: pathlib.Path, scratch: pathlib.Path) -> dict[str, bool]:
    selected = json.loads(composition.read_text(encoding="utf-8"))
    controlled_dir = installed(scratch / "rpc-controlled", controlled=True)
    controlled_path = controlled_dir / "composition.json"
    controlled = json.loads(controlled_path.read_text(encoding="utf-8"))
    shell_package = next(
        item for item in selected["packages"] if item["descriptor"]["package"] == "coding-tools"
    ).copy()
    shell_package["library"] = str(composition.parent / shell_package["library"])
    shell_package["config"] = {}
    controlled["packages"].append(shell_package)
    controlled["roles"].update(
        {role: "coding-tools" for role in shell_package["descriptor"]["provides"]}
    )
    controlled_path.write_text(json.dumps(controlled), encoding="utf-8")
    run(
        ["cargo", "test", "--locked", "-p", "eden-cli", "--test", "rpc", "--", "--ignored"],
        ROOT,
        env={**os.environ, "EDEN_RPC_TEST_COMPOSITION": str(controlled_path)},
        timeout=120,
    )
    extension_path = composition.parent / library("author_entrypoint_extension")
    shutil.copy2(author_artifact("entrypoint-extension"), extension_path)
    roles = [
        "example.entry-catalog.v1",
        "example.entry-command.v1",
        "example.entry-input.v1",
        "eden.auth.v1",
    ]
    selected["packages"].append(
        package("entrypoint-extension", roles, str(extension_path), target())
    )
    selected["roles"].update({role: "entrypoint-extension" for role in roles})
    for item in selected["packages"]:
        if item["descriptor"]["package"] == "contributions":
            item["config"].setdefault("commands", []).append(
                {"catalog": roles[0], "execute": roles[1]}
            )
            item["config"].setdefault("input_hooks", []).append(roles[2])
    composition = composition.parent / "entrypoints-extension.json"
    composition.write_text(json.dumps(selected), encoding="utf-8")
    cli_dir = scratch / "entry-cli"
    rpc_dir = scratch / "entry-rpc"
    sdk_dir = scratch / "entry-sdk"
    for path in (cli_dir, rpc_dir, sdk_dir):
        path.mkdir()
    global_dir = scratch / "entry-global"
    common = [str(host), "--composition", str(composition), "--global-dir", str(global_dir)]
    cli_history = cli_dir / "history.jsonl"
    completed = subprocess.run(
        [*common, "--cwd", str(cli_dir), "--session", str(cli_history), "--json", "write marker"],
        input="stdin context\n",
        text=True,
        encoding="utf-8",
        capture_output=True,
        timeout=30,
        check=True,
    )
    assert "\x1b" not in completed.stdout
    cli_events = [json.loads(line) for line in completed.stdout.splitlines()]
    assert any(event["kind"] == "settled" for event in cli_events)
    rpc_history = rpc_dir / "history.jsonl"
    process = subprocess.Popen(
        [*common, "--cwd", str(rpc_dir), "--session", str(rpc_history), "rpc"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    incoming: queue.Queue[dict[str, Any] | None] = queue.Queue()
    assert process.stdout is not None
    output = process.stdout

    def reader() -> None:
        for line in output:
            incoming.put(json.loads(line))
        incoming.put(None)

    worker = threading.Thread(target=reader, daemon=True)
    worker.start()
    try:
        ready = incoming.get(timeout=15)
        assert ready and ready["type"] == "ready", ready
        identity = ready["session_id"]
        assert process.stdin is not None
        process.stdin.write(
            json.dumps(
                {
                    "version": 1,
                    "id": "prompt",
                    "session_id": identity,
                    "method": "prompt",
                    "params": {"text": "stdin context\n\nwrite marker"},
                }
            )
            + "\r\n"
        )
        process.stdin.flush()
        accepted = False
        hook_observed = False
        while True:
            message = incoming.get(timeout=15)
            assert message, "RPC ended before settlement"
            if message["type"] == "event" and message["event"]["kind"] == "external_input_hook":
                hook_observed = True
            if message["type"] == "accepted" and message.get("id") == "prompt":
                accepted = True
            if message["type"] == "event" and message["event"]["kind"] == "settled":
                assert message["event"]["payload"]["outcome"]["status"] == "completed", message
                break
        assert accepted and hook_observed

        def send(identity_key: str, method: str, params: dict[str, Any]) -> None:
            assert process.stdin is not None
            process.stdin.write(
                json.dumps(
                    {
                        "version": 1,
                        "id": identity_key,
                        "session_id": identity,
                        "method": method,
                        "params": params,
                    }
                )
                + "\n"
            )
            process.stdin.flush()

        send("cap", "capabilities", {"interactions": True})
        send("dialog", "command", {"name": "ask-host", "arguments": {}})
        dialog_run = None
        while True:
            message = incoming.get(timeout=15)
            assert message
            if message["type"] == "accepted" and message.get("id") == "dialog":
                dialog_run = message["run_id"]
            if message["type"] == "event" and message["event"]["kind"] == "interaction_requested":
                send(
                    "reply",
                    "interaction.respond",
                    {
                        "interaction_id": message["event"]["payload"]["interaction_id"],
                        "value": True,
                    },
                )
            if (
                message["type"] == "event"
                and message["event"]["kind"] == "settled"
                and message["event"]["run_id"] == dialog_run
            ):
                assert message["event"]["payload"]["outcome"] == {
                    "status": "completed",
                    "value": True,
                }, message
                break
        send("auth", "auth.request", {"action": "start", "provider": "fixture"})
        private_seen = False
        auth_settled = False
        while not (private_seen and auth_settled):
            message = incoming.get(timeout=15)
            assert message
            if message["type"] == "private_result" and message.get("id") == "auth":
                assert "PRIVATE_AUTH_CANARY" in json.dumps(message)
                private_seen = True
            else:
                assert "PRIVATE_AUTH_CANARY" not in json.dumps(message), message
            if (
                message["type"] == "event"
                and message.get("id") == "auth"
                and message["event"]["kind"] == "settled"
            ):
                auth_settled = True
        send(
            "shell",
            "shell",
            {
                "command": "printf CANCELLED_MARKER; sleep 60",
                "shell": "bash",
                "exclude_from_context": False,
            },
        )
        shell_run = None
        while True:
            message = incoming.get(timeout=15)
            assert message
            if message["type"] == "accepted" and message.get("id") == "shell":
                shell_run = message["run_id"]
            if message["type"] == "event" and message["event"]["kind"] == "user_shell_output":
                assert shell_run is not None
                send("cancel-shell", "shell.cancel", {"run_id": shell_run})
            if (
                message["type"] == "event"
                and message["event"]["kind"] == "settled"
                and message["event"]["run_id"] == shell_run
            ):
                terminal = message["event"]["payload"]
                assert terminal["outcome"]["status"] == "cancelled", terminal
                assert "CANCELLED_MARKER" in terminal["partial_result"]["text"], terminal
                artifact = terminal["partial_result"]["artifacts"][0]["path"]
                assert pathlib.Path(artifact).read_bytes() == b"CANCELLED_MARKER"
                break
        send("history", "history", {})
        while True:
            message = incoming.get(timeout=15)
            assert message
            if message["type"] == "result" and message.get("id") == "history":
                assert "PRIVATE_AUTH_CANARY" not in json.dumps(message)
                shell_record = next(
                    record for record in message["result"] if record["kind"] == "user_shell"
                )
                assert "CANCELLED_MARKER" in shell_record["payload"]["result"]["text"]
                break
        process.stdin.close()
        assert process.wait(timeout=15) == 0
        worker.join(timeout=5)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    run(
        [author_artifact("embedded-client"), composition, sdk_dir],
        ROOT,
        env=os.environ.copy(),
    )
    for path in (cli_dir, rpc_dir, sdk_dir / "first", sdk_dir / "second"):
        assert (path / "marker.txt").read_text(encoding="utf-8") == "from caller", path
    return {
        "cli_rpc_sdk_file_semantics": True,
        "rpc_acceptance_and_settlement": True,
        "external_sdk_injection_and_reopen": True,
        "native_extension_hook_and_dialog": True,
    }
