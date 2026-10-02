#!/usr/bin/env python3
"""Check immutable reader stop wakeup, response drain and captured audit contracts."""

import argparse
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from session_fixture import streaming, trace_rows, wait_until


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--history", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--assert-budgets", action="store_true")
    args = parser.parse_args()
    results = []
    for mode in ("no-poll", "accepted-poll", "disconnected-poll"):
        with tempfile.TemporaryDirectory(prefix="eden-reader-stop-") as tmp:
            root = Path(tmp)
            history = root / "history.jsonl"
            shutil.copy2(args.history, history)
            endpoint = root / "endpoint.json"
            trace = root / "latency.jsonl"
            host = subprocess.Popen(
                [
                    str(args.installation.resolve() / "bin/eden"),
                    "--global-dir",
                    str(root / "global"),
                    "--offline-startup",
                    "read",
                    str(history),
                    "--endpoint",
                    str(endpoint),
                ],
                cwd=root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env={
                    **os.environ,
                    "EDEN_TUI_STATE_DIR": str(root / "state"),
                    "EDEN_LATENCY_TRACE": str(trace),
                },
            )
            connection = None
            try:
                wait_until(endpoint.exists, seconds=20)
                ep = json.loads(endpoint.read_text())
                identity = streaming.live.call(ep, "/snapshot")
                assert "history" not in identity
                assert identity["state"]["session_id"] == ep["session_id"]
                if mode != "no-poll":
                    address, port = ep["address"].rsplit(":", 1)
                    connection = socket.create_connection((address, int(port)), timeout=15)
                    connection.sendall(
                        f"GET /tui/snapshot?after=0&history_view=presentation HTTP/1.1\r\nHost: localhost\r\nX-Eden-Token: {ep['token']}\r\nConnection: close\r\n\r\n".encode()
                    )
                    wait_until(
                        lambda trace=trace: any(
                            row.get("stage") == "reader.wait" and row.get("edge") == "start"
                            for row in trace_rows(trace)
                        ),
                        seconds=10,
                    )
                    if mode == "disconnected-poll":
                        connection.close()
                        connection = None
                started = time.monotonic()
                streaming.live.call(ep, "/shutdown", {})
                reply = time.monotonic()
                drained = 0
                if connection:
                    response = bytearray()
                    while chunk := connection.recv(1024 * 1024):
                        response.extend(chunk)
                    headers, body = response.split(b"\r\n\r\n", 1)
                    declared = int(
                        next(
                            line.split(b":", 1)[1]
                            for line in headers.split(b"\r\n")
                            if line.lower().startswith(b"content-length:")
                        )
                    )
                    assert len(body) == declared
                    assert json.loads(body)["ok"] is True
                    drained = len(body)
                host.wait(timeout=10)
                elapsed = time.monotonic() - started
                row = {
                    "mode": mode,
                    "shutdown_reply_seconds": reply - started,
                    "shutdown_to_exit_seconds": elapsed,
                    "endpoint_retired": not endpoint.exists(),
                    "exit_code": host.returncode,
                    "response_drained_bytes": drained,
                    "metadata_bytes": len(json.dumps(identity).encode()),
                }
                assert row["endpoint_retired"] and row["exit_code"] == 0
                if args.assert_budgets:
                    assert elapsed < 1.0, "reader stop exceeded one-second checkpoint"
                results.append(row)
            finally:
                if connection:
                    connection.close()
                if host.poll() is None:
                    host.kill()
                    host.wait()
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results))


if __name__ == "__main__":
    main()
