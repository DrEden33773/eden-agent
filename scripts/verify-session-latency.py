#!/usr/bin/env python3
"""Measure history costs and prove Ctrl+C twice returns input to a real interactive shell.

All hosts, histories and model credentials belong to an isolated fixture. Timings
separate reservation, durable acceptance, provider arrival, written frames,
cancellation settlement, frontend readiness and shell command execution.
"""

import argparse
import http.client
import json
import os
import shlex
import subprocess
import threading
import time
from pathlib import Path

from session_fixture import (
    Fixture,
    Terminal,
    owner,
    ready_view,
    records,
    select,
    streaming,
    trace_rows,
    wait_until,
)


def call(endpoint, route, body=None):
    connection = http.client.HTTPConnection(endpoint["address"], timeout=180)
    started = time.monotonic()
    connection.request(
        "POST" if body is not None else "GET",
        route,
        body=json.dumps(body).encode() if body is not None else None,
        headers={"X-Eden-Token": endpoint["token"], "Content-Type": "application/json"},
    )
    response = connection.getresponse()
    first_byte = time.monotonic()
    data = response.read()
    transferred = time.monotonic()
    connection.close()
    value = json.loads(data)
    decoded = time.monotonic()
    assert response.status == 200 and value["ok"], value.get("error")
    return value["result"], {
        "seconds": decoded - started,
        "headers_seconds": first_byte - started,
        "transfer_seconds": transferred - first_byte,
        "decode_seconds": decoded - transferred,
        "bytes": len(data),
    }


def history(fixture, mib, *, visible=False, shape="padding"):
    path = fixture.root / ".eden/sessions" / f"latency-{mib}.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    seed = records(fixture.history)
    if visible:
        seed.append(
            {
                "schema_version": 2,
                "session_id": seed[0]["session_id"],
                "sequence": len(seed) + 1,
                "parent_id": len(seed),
                "branch": "main",
                "run_id": 0,
                "kind": "message",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "text", "text": "LATENCY_VISIBLE_SEED"}],
                },
            }
        )
    with path.open("w") as output:
        output.write(json.dumps({"schema_version": 2, "transaction": seed}) + "\n")
        for index in range(mib):
            sequence = len(seed) + index + 1
            record = {
                "schema_version": 2,
                "session_id": seed[0]["session_id"],
                "sequence": sequence,
                "parent_id": sequence - 1,
                "branch": "main",
                "run_id": 0,
                "kind": "user_message" if shape == "padding" else "model_request",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "text", "text": f"LATENCY_{index}"}],
                    "fixture_padding": "x" * 1048576,
                },
            }
            if shape == "audit":
                record["payload"] = {
                    "input": {
                        "items": [
                            {
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "text", "text": "audit prefix " * 80660}],
                            }
                        ],
                        "tools": [],
                    },
                    "prompt_meta": {"prompt_id": "archived", "attempt": index},
                }
            output.write(json.dumps({"schema_version": 2, "transaction": [record]}) + "\n")
    return path


def native(installation, output, mib, shape):
    output.mkdir(parents=True, exist_ok=True)
    with Fixture(installation, output) as fixture:
        path = history(fixture, mib, shape=shape)
        endpoint_path = fixture.root / "measured-endpoint.json"
        process = subprocess.Popen(
            [
                *fixture.arguments(),
                "--session",
                str(path),
                "resume-live",
                "--endpoint",
                str(endpoint_path),
            ],
            cwd=fixture.root,
            env={**os.environ, "EDEN_TUI_STATE_DIR": str(fixture.state)},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            wait_until(lambda: endpoint_path.exists() or process.poll() is not None, seconds=180)
            assert endpoint_path.exists(), process.communicate()[1].decode()
            endpoint = json.loads(endpoint_path.read_text())
            snapshots = [call(endpoint, "/tui/snapshot")[1] for _ in range(2)]
            presentation = [
                call(endpoint, "/tui/snapshot?history_view=presentation")[1] for _ in range(2)
            ]
            fixture.provider.started.clear()
            fixture.provider.allow.clear()
            fixture.provider.mode = "single"
            fixture.provider.gate_timeout = 180
            started = time.monotonic()
            reply, reservation = call(
                endpoint, "/prompt", {"request_id": "latency-native", "text": "LATENCY_PROBE"}
            )
            assert fixture.provider.started.wait(180), "Provider never received the request"
            provider = fixture.provider.arrivals[-1] - started
            cancelled = time.monotonic()
            _, cancel = call(endpoint, "/cancel", {"run_id": reply["run_id"]})
            terminal, _ = call(endpoint, "/terminal", {"run_id": reply["run_id"]})
            settlement = time.monotonic() - cancelled
            fixture.provider.allow.set()
            assert terminal["outcome"]["status"] == "cancelled", terminal
            assert len(fixture.provider.requests) == 1
            return {
                "scenario": "native",
                "mib": mib,
                "history_shape": shape,
                "history_bytes": path.stat().st_size,
                "session": endpoint["session_id"],
                "run": reply["run_id"],
                "snapshot": snapshots,
                "presentation_snapshot": presentation,
                "reservation": reservation,
                "provider_seconds": provider,
                "cancel": cancel,
                "cancel_to_terminal_seconds": settlement,
                "provider_requests": len(fixture.provider.requests),
            }
        finally:
            if endpoint_path.exists() and process.poll() is None:
                call(json.loads(endpoint_path.read_text()), "/shutdown", {})
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def prompt_frames(trace, offset, prompt=None):
    return [
        row
        for row in trace_rows(trace)[offset:]
        if row.get("direction") == "frame"
        and row.get("method") == "eden/turn/state"
        and (prompt is None or row.get("prompt") == prompt)
    ]


def frontend_ready(trace, terminal, offset, completion):
    """Require a frame written after the matching prompt result, then drain that frame."""
    frame = wait_until(
        lambda: next(
            (
                row
                for row in trace_rows(trace)[offset:]
                if row.get("direction") == "frame"
                and row.get("time_ns", 0) >= completion["time_ns"]
                and (
                    completion.get("session") is None or row.get("session") == completion["session"]
                )
                and (
                    (
                        row.get("method") == "session/prompt/ready"
                        and row.get("idle") is True
                        and (
                            completion.get("prompt") is None
                            or row.get("prompt") == completion["prompt"]
                        )
                    )
                    or (row.get("method") == "eden/turn/state" and row.get("phase") == "idle")
                )
            ),
            None,
        ),
        terminal,
        seconds=180,
    )
    terminal.drain()
    return frame


def shell_terminal(fixture, trace):
    """util-linux script gives Bash its controlling tty and normal foreground job control."""
    shell = fixture.root / "shell.rc"
    shell.write_text("PS1='EDEN_SHELL> '\nPROMPT_COMMAND=\n")
    terminal = Terminal(
        fixture.installation / "bin/eden",
        fixture.endpoint_file,
        width=110,
        height=40,
        command=[
            "script",
            "--quiet",
            "--return",
            "--command",
            f"bash --noprofile --rcfile {shlex.quote(str(shell))} -i",
            "/dev/null",
        ],
        env={
            "EDEN_TUI_STATE_DIR": str(fixture.state),
            "EDEN_FRONTEND_TRACE": str(trace.resolve()),
            "GROK_XAI_API_BASE_URL": f"http://127.0.0.1:{fixture.business.server_port}",
        },
    )
    fixture.terminal = terminal
    wait_until(lambda: b"EDEN_SHELL> " in terminal.output, terminal)
    terminal.command(shlex.join(fixture.arguments()))
    wait_until(trace.exists, terminal, seconds=180)
    ready_view(terminal, trace, 0)
    return terminal


def shell_exit(terminal, output, trace):
    first = time.monotonic()
    terminal.send(b"\x03")
    second = time.monotonic()
    output_offset = len(terminal.output)
    terminal.send(b"\x03")
    wait_until(lambda: b"EDEN_SHELL> " in terminal.output[output_offset:], terminal, seconds=180)
    # The complete marker is absent from the input command, so tty echo cannot satisfy this.
    marker = f"SHELL_{time.monotonic_ns()}_READY"
    prefix, nonce, suffix = marker.split("_")
    terminal.command(f"printf '%s_%s_%s\\n' {prefix} {nonce} {suffix}")
    wait_until(lambda: (marker + "\r\n").encode() in terminal.output, terminal, seconds=180)
    finished = time.monotonic()
    (output / "shell-exit.ansi").write_bytes(terminal.output)
    frames = trace_rows(trace)
    terminal.command("exit")
    wait_until(lambda: terminal.process.poll() is not None, terminal)
    terminal.close(screen=False)
    return {
        "ctrl_c_count": 2,
        "first_to_second_seconds": second - first,
        "second_ctrl_c_to_shell_command_seconds": finished - second,
        "shell_marker_executed": True,
        "shell_marker": marker,
        "last_frame": frames[-1] if frames else None,
    }


def frontend(installation, output, mib, active_exit, shape, round_count):
    output.mkdir(parents=True, exist_ok=True)
    trace = output / "wire.jsonl"
    with Fixture(installation, output) as fixture:
        path = history(fixture, mib, visible=True, shape=shape)
        terminal = shell_terminal(fixture, trace)
        opened = time.monotonic()
        terminal.command("/resume")
        wait_until(
            lambda: any(
                row.get("direction") == "frame"
                and row.get("method") == "directory/accepted"
                and row.get("count", 0) > 0
                for row in trace_rows(trace)
            ),
            terminal,
            seconds=180,
        )
        directory_seconds = time.monotonic() - opened
        terminal.send(b"/" + path.stem.encode())
        terminal.wait(path.stem[:18])
        terminal.send(b"\x1b[B")
        previewed = time.monotonic()
        terminal.send(b"d")
        terminal.wait("Move session to Trash?", seconds=180)
        terminal.drain()
        preview_seconds = time.monotonic() - previewed
        fixture.capture("removal-preview")
        terminal.send(b"\x1b")
        terminal.wait("Resume session")
        terminal.send(b"\x1b")
        terminal.wait("/ to search")
        selected = select(terminal, path, trace)
        _, endpoint = owner(fixture, path)
        fixture.provider.mode = "single"
        fixture.provider.gate_timeout = 180
        rounds = []
        cancellation = None
        for index in range(round_count):
            fixture.provider.started.clear()
            fixture.provider.done.clear()
            fixture.provider.allow.clear()
            offset = len(trace_rows(trace))
            request_count = len(fixture.provider.requests)
            started = time.monotonic()
            started_ns = time.time_ns()
            terminal.command(f"LATENCY_ROUND_{index}")
            wait_until(fixture.provider.started.is_set, terminal, seconds=180)
            provider_seconds = fixture.provider.arrivals[-1] - started
            requests = [
                row
                for row in trace_rows(trace)[offset:]
                if row.get("direction") == "in" and row.get("method") == "session/prompt"
            ]
            assert len(requests) == 1, requests
            if active_exit and index == round_count - 1:
                state, _ = call(endpoint, "/snapshot")
                run = state["state"]["active_run"]
                assert run is not None
                cancelled = time.monotonic()
                terminal.send(b"\x03")
                result, _ = call(endpoint, "/terminal", {"run_id": run})
                settled = time.monotonic()
                assert result["outcome"]["status"] == "cancelled", result
                completion = wait_until(
                    lambda offset=offset, requests=requests: next(
                        (
                            row
                            for row in trace_rows(trace)[offset:]
                            if row.get("direction") == "out" and row.get("id") == requests[0]["id"]
                        ),
                        None,
                    ),
                    terminal,
                    seconds=180,
                )
                ready = frontend_ready(trace, terminal, offset, completion)
                cancellation = {
                    "run": run,
                    "provider_seconds": provider_seconds,
                    "cancel_to_terminal_seconds": settled - cancelled,
                    "cancel_to_frontend_ready_seconds": time.monotonic() - cancelled,
                }
                cancellation["ready_frame"] = ready
                (output / "cancellation.json").write_text(json.dumps(cancellation, indent=2) + "\n")
                terminal.send(b"\x15")
                exit_result = shell_exit(terminal, output, trace)
                fixture.provider.allow.set()
                break
            released = time.monotonic()
            fixture.provider.allow.set()
            completion = wait_until(
                lambda offset=offset, requests=requests: next(
                    (
                        row
                        for row in trace_rows(trace)[offset:]
                        if row.get("direction") == "out" and row.get("id") == requests[0]["id"]
                    ),
                    None,
                ),
                terminal,
                seconds=180,
            )
            frames = prompt_frames(trace, offset)
            state = next((row for row in frames if row.get("prompt")), None)
            assert state is not None, "Missing written prompt identity"
            samples = prompt_frames(trace, offset, state["prompt"])
            timers = [row["elapsed_ms"] for row in samples if row.get("elapsed_ms") is not None]
            resets = [
                (left, right)
                for left, right in zip(timers, timers[1:], strict=False)
                if right < left
            ]
            display_timers = [
                row["display_elapsed_ms"]
                for row in samples
                if row.get("display_elapsed_ms") is not None
            ]
            display_resets = [
                (left, right)
                for left, right in zip(display_timers, display_timers[1:], strict=False)
                if right < left
            ]
            visible = [
                row
                for row in trace_rows(trace)[offset:]
                if row.get("direction") == "frame"
                and row.get("method") == "session/update"
                and row.get("update") == "agent_message_chunk"
                and row.get("replay") is False
            ]
            assert completion["error"] is None, completion
            assert len(fixture.provider.requests) == request_count + 1
            ready = frontend_ready(trace, terminal, offset, completion)
            rounds.append(
                {
                    "prompt": state["prompt"],
                    "provider_seconds": provider_seconds,
                    "first_byte_seconds": fixture.provider.first_bytes[-1] - started,
                    "first_written_response_seconds": (visible[0]["time_ns"] - started_ns) / 1e9
                    if visible
                    else None,
                    "provider_release_to_frontend_complete_seconds": time.monotonic() - released,
                    "timer_resets": resets,
                    "display_timer_resets": display_resets,
                    "display_timer_samples": len(display_timers),
                    "acceptance_wait_frames": sum(row["phase"] == "acceptance" for row in samples),
                    "written_response_frames": visible,
                    "phases": list(dict.fromkeys(row["phase"] for row in samples)),
                    "ready_frame": ready,
                }
            )
        else:
            exit_result = shell_exit(terminal, output, trace)
        return {
            "scenario": "frontend-active-exit" if active_exit else "frontend-idle-exit",
            "mib": mib,
            "history_shape": shape,
            "history_bytes": path.stat().st_size,
            "directory_seconds": directory_seconds,
            "removal_preview_seconds": preview_seconds,
            "selected_load": selected,
            "rounds": rounds,
            "cancellation": cancellation,
            "exit": exit_result,
            "provider_requests": len(fixture.provider.requests),
            "grok_business_http_calls": len(fixture.business.calls),
        }


class FeedbackRelay(streaming.Relay):
    """Hold the next snapshot response until a written disconnect frame releases it."""

    def __init__(self, address):
        super().__init__(address)
        self.RequestHandlerClass = FeedbackHandler
        self.gate: tuple[threading.Event, threading.Event] | None = None


class FeedbackHandler(streaming.RelayHandler):
    def forward(self):
        relay = self.server
        assert isinstance(relay, FeedbackRelay)
        gate = relay.gate if self.path.startswith("/tui/snapshot") else None
        if gate is not None:
            arrived, release = gate
            arrived.set()
            assert release.wait(40), "snapshot gate never released"
        super().forward()


def feedback(installation, output):
    output.mkdir(parents=True, exist_ok=True)
    trace = output / "wire.jsonl"
    with Fixture(installation, output) as fixture:
        relay = FeedbackRelay(fixture.endpoint["address"])
        thread = threading.Thread(target=relay.serve_forever, daemon=True)
        thread.start()
        try:
            endpoint = fixture.root / "feedback-endpoint.json"
            endpoint.write_text(
                json.dumps({**fixture.endpoint, "address": f"127.0.0.1:{relay.server_port}"})
            )
            terminal = Terminal(
                installation / "bin/eden",
                endpoint,
                command=[*fixture.arguments(), "tui", "--endpoint", str(endpoint)],
                env={
                    "EDEN_FRONTEND_TRACE": str(trace.resolve()),
                    "EDEN_TUI_STATE_DIR": str(fixture.state),
                },
            )
            fixture.terminal = terminal
            wait_until(trace.exists, terminal)
            ready_view(terminal, trace, 0)
            fixture.provider.mode = "single"
            fixture.provider.gate_timeout = 90
            fixture.provider.started.clear()
            fixture.provider.allow.clear()
            offset = len(trace_rows(trace))
            terminal.command("FEEDBACK_RECONNECT_PROBE")
            wait_until(fixture.provider.started.is_set, terminal)
            request = next(
                row
                for row in trace_rows(trace)[offset:]
                if row.get("direction") == "in" and row.get("method") == "session/prompt"
            )
            cycles = []
            for _ in range(2):
                gate = (threading.Event(), threading.Event())
                relay.gate = gate
                wait_until(gate[0].is_set, terminal)
                cycle_offset = len(trace_rows(trace))
                disconnected = wait_until(
                    lambda cycle_offset=cycle_offset: next(
                        (
                            row
                            for row in trace_rows(trace)[cycle_offset:]
                            if row.get("direction") == "frame"
                            and row.get("method") == "eden/connection/state"
                            and row.get("connected") is False
                        ),
                        None,
                    ),
                    terminal,
                    seconds=20,
                )
                terminal.drain()
                assert "mutation acceptance unknown" not in terminal.display
                updates = [
                    row
                    for row in trace_rows(trace)[cycle_offset:]
                    if row.get("direction") == "event"
                    and row.get("update") == "agent_message_chunk"
                ]
                assert not updates, "connection feedback entered the assistant transcript"
                fixture.capture(f"disconnected-{len(cycles)}")
                relay.gate = None
                gate[1].set()
                connected = wait_until(
                    lambda cycle_offset=cycle_offset: next(
                        (
                            row
                            for row in trace_rows(trace)[cycle_offset:]
                            if row.get("direction") == "frame"
                            and row.get("method") == "eden/connection/state"
                            and row.get("connected") is True
                        ),
                        None,
                    ),
                    terminal,
                    seconds=20,
                )
                assert connected["session"] == disconnected["session"]
                cycles.append(
                    {
                        "disconnect": disconnected,
                        "reconnect": connected,
                        "assistant_chunks_while_held": len(updates),
                    }
                )
            fixture.provider.allow.set()
            completion = wait_until(
                lambda: next(
                    (
                        row
                        for row in trace_rows(trace)[offset:]
                        if row.get("direction") == "out" and row.get("id") == request["id"]
                    ),
                    None,
                ),
                terminal,
            )
            ready = frontend_ready(trace, terminal, offset, completion)
            timers = [
                row["display_elapsed_ms"]
                for row in prompt_frames(trace, offset)
                if row.get("display_elapsed_ms") is not None
            ]
            assert timers and all(
                right >= left for left, right in zip(timers, timers[1:], strict=False)
            ), timers
            assert len(fixture.provider.requests) == 1
            snapshot, _ = call(fixture.endpoint, "/tui/snapshot")
            assistant = [
                record["payload"]
                for record in snapshot["history"]
                if record["kind"] == "message" and record["payload"].get("role") == "assistant"
            ]
            assert assistant and all(
                "Host disconnected" not in json.dumps(item)
                and "Reconnected" not in json.dumps(item)
                for item in assistant
            )
            fixture.capture("reconnected-completed")
            return {
                "scenario": "feedback",
                "cycles": cycles,
                "ready_frame": ready,
                "provider_requests": len(fixture.provider.requests),
                "display_timer_nondecreasing": True,
            }
        finally:
            if relay.gate:
                relay.gate[1].set()
            relay.shutdown()
            relay.server_close()
            thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mib", type=int, nargs="+", default=[0, 64, 200])
    parser.add_argument("--shape", choices=["padding", "audit"], default="padding")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument(
        "--scenarios",
        nargs="+",
        choices=["native", "idle", "active", "feedback"],
        default=["native", "idle", "active", "feedback"],
    )
    parser.add_argument(
        "--observe",
        action="store_true",
        help="Collect baseline failures without declaring a regression pass",
    )
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    os.environ["EDEN_LATENCY_TRACE"] = str((args.output / "latency.jsonl").resolve())
    results = []
    for scenario in args.scenarios:
        if scenario == "feedback":
            results.append(feedback(args.installation, args.output / scenario))
            (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
            continue
        for mib in args.mib:
            output = args.output / f"{scenario}-{mib}"
            row = (
                native(args.installation, output, mib, args.shape)
                if scenario == "native"
                else frontend(
                    args.installation, output, mib, scenario == "active", args.shape, args.rounds
                )
            )
            results.append(row)
            print(json.dumps(row), flush=True)
            (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    failures = []
    for row in results:
        if row["scenario"] == "feedback":
            continue
        if row["scenario"] == "native":
            if max(item["bytes"] for item in row["presentation_snapshot"]) > 1048576:
                failures.append(f"{row['mib']} MiB: presentation copied audit-only payload")
            if row["provider_seconds"] > 5:
                failures.append(f"{row['mib']} MiB: local provider preparation exceeds 5s")
            if row["cancel_to_terminal_seconds"] > 1:
                failures.append(f"{row['mib']} MiB: cancellation settlement exceeds 1s")
        else:
            if any(round_["provider_seconds"] > 5 for round_ in row["rounds"]):
                failures.append(
                    f"{row['scenario']} {row['mib']} MiB: local provider preparation exceeds 5s"
                )
            if any(
                round_["timer_resets"]
                or round_["display_timer_resets"]
                or round_["acceptance_wait_frames"]
                for round_ in row["rounds"]
            ):
                failures.append(
                    f"{row['scenario']} {row['mib']} MiB: timer reset or acceptance wait"
                )
            if not args.observe and any(
                round_["display_timer_samples"] == 0 for round_ in row["rounds"]
            ):
                failures.append(
                    f"{row['scenario']} {row['mib']} MiB: no written display timer samples"
                )
            if row["cancellation"] and (
                row["cancellation"]["provider_seconds"] > 5
                or row["cancellation"]["cancel_to_terminal_seconds"] > 1
            ):
                failures.append(
                    f"{row['scenario']} {row['mib']} MiB: active preparation or cancellation exceeded its budget"
                )
            if row["exit"]["second_ctrl_c_to_shell_command_seconds"] > 2:
                failures.append(
                    f"{row['scenario']} {row['mib']} MiB: shell input return exceeds 2s"
                )
    (args.output / "verdict.json").write_text(
        json.dumps({"observational": args.observe, "failures": failures}, indent=2) + "\n"
    )
    if not args.observe:
        assert not failures, failures


if __name__ == "__main__":
    main()
