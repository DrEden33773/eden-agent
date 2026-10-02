#!/usr/bin/env python3
"""Hold real work preflight across terminal exit and observe host ownership."""

import argparse
import json
import socket
import threading
from pathlib import Path

from install import package, target
from session_fixture import (
    Fixture,
    exited,
    probe,
    process_gone,
    records,
    trace_rows,
    wait_until,
    writer_held,
)

CATALOG = "eden.model-catalog.v1"


class ResolveGate:
    """A socket receipt identifies the reserved run; a reply releases its preflight."""

    def __enter__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(30)
        self.address = "127.0.0.1:" + str(self.listener.getsockname()[1])
        self.arrived = threading.Event()
        self.release = threading.Event()
        self.reply = "reject"
        self.run = None
        self.error = None

        def serve():
            try:
                with self.listener.accept()[0] as connection:
                    with connection.makefile("rb") as reader:
                        self.run = int(reader.readline())
                    self.arrived.set()
                    assert self.release.wait(60), "gate was never released"
                    connection.sendall((self.reply + "\n").encode())
            except Exception as error:
                self.error = error
                self.arrived.set()

        self.thread = threading.Thread(target=serve, daemon=True)
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.release.set()
        self.listener.close()
        self.thread.join(timeout=35)


def scenario(installation, author, output, case):
    output.mkdir(parents=True, exist_ok=True)
    trace = output / "wire.jsonl"
    # Release preflight before Fixture teardown tries to stop its isolated host.
    with Fixture(installation, output) as fixture, ResolveGate() as gate:
        composition = json.loads(fixture.composition.read_text())
        model_access = next(
            p for p in composition["packages"] if p["descriptor"]["package"] == "model-access"
        )
        target_path = fixture.root / "target.json"
        target_path.write_text(json.dumps(model_access["config"]["catalog"]["models"][0]))
        enable = fixture.root / "resolve-gate"
        composition["packages"].append(
            package(
                "model-services",
                [
                    "eden.coding-provider.v1",
                    CATALOG,
                    "eden.model-manager.v1",
                    "eden.credential-source.v1",
                ],
                str(author.resolve()),
                target(),
                {"target_path": str(target_path), "resolve_gate": str(enable)},
            )
        )
        composition["roles"][CATALOG] = "model-services"
        configuration = fixture.root / "gated.json"
        configuration.write_text(json.dumps(composition))
        terminal = fixture.start(composition=configuration, trace=trace)
        registration_path = next(
            p
            for p in (fixture.state / "live").glob("*.json")
            if json.loads(p.read_text()).get("history") != str(fixture.history)
        )
        registration = json.loads(registration_path.read_text())
        history = Path(registration["history"])
        endpoint_path = Path(registration["endpoint"])
        endpoint = json.loads(endpoint_path.read_text())
        enable.write_text(gate.address)
        fixture.provider.allow.clear()
        terminal.command("FIRST_PENDING_ADMISSION")
        assert gate.arrived.wait(15), terminal.display
        assert gate.error is None, gate.error
        frame = probe.streaming.live.call(endpoint, "/tui/snapshot")
        assert frame["state"]["active_run"] == gate.run, frame["state"]
        events = [e for e in frame["events"] if e["run_id"] == gate.run]
        assert any(e["kind"] == "reserved" for e in events), events
        assert not any(e["kind"] == "accepted" for e in events), events
        assert not history.exists() and not fixture.provider.requests
        if case == "timer":
            wait_until(
                lambda: any(
                    row.get("direction") == "frame"
                    and row.get("method") == "eden/turn/state"
                    and row.get("phase") == "acceptance"
                    for row in trace_rows(trace)
                ),
                terminal,
                seconds=20,
            )
            gate.reply = "accept"
            gate.release.set()
            wait_until(fixture.provider.started.is_set, terminal)
            fixture.provider.allow.set()
            wait_until(
                lambda: any(
                    row.get("direction") == "frame"
                    and row.get("method") == "session/prompt/ready"
                    and row.get("idle") is True
                    for row in trace_rows(trace)
                ),
                terminal,
            )
            samples = [
                row["display_elapsed_ms"]
                for row in trace_rows(trace)
                if row.get("direction") == "frame"
                and row.get("method") == "eden/turn/state"
                and row.get("display_elapsed_ms") is not None
            ]
            assert samples and all(
                right >= left for left, right in zip(samples, samples[1:], strict=False)
            ), samples
            assert len(fixture.provider.requests) == 1
            fixture.capture("delayed-admission-timer")
            exited(terminal)
            return {
                "case": case,
                "reserved_run": gate.run,
                "display_timer_nondecreasing": True,
                "provider_requests": len(fixture.provider.requests),
                "passed": True,
            }
        peer = None
        if case == "peer":
            peer = probe.streaming.live.call(endpoint, "/attach", {"frontend": "tui"})["attachment"]
        exited(terminal)
        assert not process_gone(endpoint["pid"]), "pending preflight was stopped by exit"
        gate.reply = "accept" if case == "accepted" else "reject"
        gate.release.set()
        if case == "rejected":
            try:
                wait_until(lambda: process_gone(endpoint["pid"]), seconds=10)
            except AssertionError:
                frame = probe.streaming.live.call(endpoint, "/tui/snapshot")
                (output / "orphan.json").write_text(
                    json.dumps(
                        {
                            "state": frame["state"],
                            "run": gate.run,
                            "history_exists": history.exists(),
                            "writer_held": writer_held(history),
                            "endpoint_exists": endpoint_path.exists(),
                            "registration_exists": registration_path.exists(),
                            "provider_requests": len(fixture.provider.requests),
                        },
                        indent=2,
                    )
                    + "\n"
                )
                raise
            wait_until(lambda: not endpoint_path.exists() and not registration_path.exists())
            assert not writer_held(history) and not history.exists()
            assert not fixture.provider.requests
        elif case == "accepted":
            assert fixture.provider.started.wait(15)
            durable = records(history)
            assert durable[0]["kind"] == "session"
            assert any(
                r["kind"] == "work_admitted"
                and "FIRST_PENDING_ADMISSION" in json.dumps(r["payload"])
                for r in durable
            )
            assert writer_held(history) and not process_gone(endpoint["pid"])
            fixture.provider.allow.set()
            probe.streaming.live.wait_for(endpoint, lambda f: f["state"]["active_run"] is None)
            assert not process_gone(endpoint["pid"])
        else:
            frame = probe.streaming.live.wait_for(
                endpoint, lambda f: f["state"]["active_run"] is None
            )
            assert not frame["state"]["closed"]
            probe.streaming.live.call(endpoint, f"/snapshot?attachment={peer}")
            assert not process_gone(endpoint["pid"]) and writer_held(history)
            assert not history.exists() and not fixture.provider.requests
            probe.streaming.live.call(endpoint, "/detach", {"attachment": peer})
            wait_until(lambda: process_gone(endpoint["pid"]), seconds=10)
            assert not writer_held(history)
        assert not fixture.business.calls
        return {"case": case, "reserved_run": gate.run, "passed": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--author", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", choices=["rejected", "accepted", "peer", "timer"])
    args = parser.parse_args()
    results = [
        scenario(args.installation, args.author, args.output / case, case)
        for case in ([args.case] if args.case else ["rejected", "accepted", "peer", "timer"])
    ]
    (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
