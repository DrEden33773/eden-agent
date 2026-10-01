#!/usr/bin/env python3
"""Observe failed frontend initialization and outstanding HTTP replies during host retirement."""

import argparse
import json
import os
import socket
import subprocess
from pathlib import Path

from install import package, target
from session_fixture import Fixture, probe, wait_until, writer_held


def failed_initialization(installation, author, output):
    with Fixture(installation, output) as fixture:
        composition = json.loads(fixture.composition.read_text())
        composition["packages"].append(
            package(
                "model-services",
                [
                    "eden.coding-provider.v1",
                    "eden.model-catalog.v1",
                    "eden.model-manager.v1",
                    "eden.credential-source.v1",
                ],
                str(author.resolve()),
                target(),
                {"target_path": str(fixture.root / "deliberately-missing-target.json")},
            )
        )
        composition["roles"]["eden.model-catalog.v1"] = "model-services"
        config = fixture.root / "failed-initialize.json"
        config.write_text(json.dumps(composition))
        terminal = probe.Terminal(
            installation / "bin/eden",
            fixture.endpoint_file,
            width=100,
            height=30,
            command=fixture.arguments(config),
            env={"EDEN_TUI_STATE_DIR": str(fixture.state)},
        )
        fixture.terminal = terminal
        wait_until(lambda: terminal.process.poll() is not None, terminal)
        assert terminal.process.returncode != 0
        terminal.close(expected_code=None, screen=False)
        fixture.capture("initialize-rejected")
        leaked = [
            json.loads(p.read_text())
            for p in (fixture.state / "live").glob("*.json")
            if json.loads(p.read_text()).get("history") != str(fixture.history)
        ]
        observations = [
            {
                "history_exists": Path(row["history"]).exists(),
                "writer_held": writer_held(Path(row["history"])),
                "endpoint_exists": Path(row["endpoint"]).exists(),
            }
            for row in leaked
        ]
        (output / "initialization.json").write_text(
            json.dumps({"remaining_hosts": observations}, indent=2) + "\n"
        )
        assert not leaked, observations
        assert not fixture.provider.requests


def drain_reader_response(installation, output):
    with Fixture(installation, output) as fixture:
        source = fixture.root / "large-reader.jsonl"
        header = json.loads(fixture.history.read_text().splitlines()[0])
        source.write_text(json.dumps(header) + "\n")
        # Larger than loopback's send buffer: once headers arrive, the full reply
        # cannot have drained while this client deliberately withholds body reads.
        payload = (
            "OUTSTANDING_RESPONSE_BEGIN" + "x" * (16 * 1024 * 1024) + "OUTSTANDING_RESPONSE_END"
        )
        with source.open("a") as stream:
            stream.write(
                json.dumps(
                    {
                        "schema_version": 2,
                        "transaction": [
                            {
                                "schema_version": 2,
                                "session_id": fixture.endpoint["session_id"],
                                "sequence": 2,
                                "parent_id": 1,
                                "branch": "main",
                                "run_id": 1,
                                "kind": "message",
                                "payload": {
                                    "type": "message",
                                    "role": "user",
                                    "content": [{"type": "text", "text": payload}],
                                },
                            }
                        ],
                    }
                )
                + "\n"
            )
        endpoint_path = fixture.root / "reader.json"
        reader = subprocess.Popen(
            [*fixture.arguments(), "read", str(source), "--endpoint", str(endpoint_path)],
            cwd=fixture.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={**os.environ, "EDEN_TUI_STATE_DIR": str(fixture.state)},
        )
        try:
            endpoint = probe.streaming.live.ready(reader, endpoint_path)
            address, port = endpoint["address"].rsplit(":", 1)
            with (
                socket.create_connection((address, int(port)), timeout=15) as connection,
                socket.create_connection((address, int(port)), timeout=15) as unfinished,
            ):
                unfinished.sendall(b"GET /tui/snapshot HTTP/1.1\r\n")
                connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
                connection.sendall(
                    (
                        f"GET /tui/snapshot HTTP/1.1\r\nHost: localhost\r\nx-eden-token: {endpoint['token']}\r\nConnection: close\r\n\r\n"
                    ).encode()
                )
                received = b""
                while b"\r\n\r\n" not in received:
                    received += connection.recv(1024)
                headers, body = received.split(b"\r\n\r\n", 1)
                length = int(
                    next(
                        line.split(b":", 1)[1]
                        for line in headers.split(b"\r\n")
                        if line.lower().startswith(b"content-length:")
                    )
                )
                assert length > 16 * 1024 * 1024
                probe.streaming.live.call(
                    endpoint, "/shutdown", {"session_id": endpoint["session_id"]}
                )
                connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
                failure = None
                chunks = [body]
                try:
                    while chunk := connection.recv(256 * 1024):
                        chunks.append(chunk)
                except OSError as error:
                    failure = str(error)
                body = b"".join(chunks)
                observation = {
                    "declared_bytes": length,
                    "received_bytes": len(body),
                    "read_error": failure,
                }
                (output / "response.json").write_text(json.dumps(observation, indent=2) + "\n")
                assert len(body) == length and failure is None, observation
                assert b"OUTSTANDING_RESPONSE_END" in body
                # The host must retire even while another peer keeps an incomplete
                # request open; request reads are cancelled independently of replies.
                assert reader.wait(timeout=15) == 0
            assert not endpoint_path.exists()
        finally:
            if reader.poll() is None:
                reader.kill()
                reader.wait()
            if reader.stdout:
                reader.stdout.close()
            if reader.stderr:
                reader.stderr.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--author", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", choices=["initialization", "response"])
    args = parser.parse_args()
    args.installation = args.installation.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    results = {}
    for case in [args.case] if args.case else ["initialization", "response"]:
        output = args.output if args.case else args.output / case
        output.mkdir(parents=True, exist_ok=True)
        if case == "initialization":
            failed_initialization(args.installation, args.author, output)
        else:
            drain_reader_response(args.installation, output)
        results[case] = "passed"
    (args.output / "summary.json").write_text(json.dumps(results) + "\n")


if __name__ == "__main__":
    main()
