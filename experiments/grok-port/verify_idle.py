#!/usr/bin/env python3
"""Measure the installed host and native pager on a fixed synthetic large history."""

import argparse
import json
import os
import statistics
import time
from collections import Counter
from pathlib import Path

from verify_workflows import ROOT, Fixture


def ticks(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def descendants(pid):
    result = [pid]
    for parent in result:
        children = Path(f"/proc/{parent}/task/{parent}/children")
        if children.exists():
            result.extend(int(child) for child in children.read_text().split())
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, default=ROOT / "artifacts/s4-g2-host")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--assert-idle", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with Fixture(args.installation, args.output, configuration_author=False) as fixture:
        for _ in range(80):
            fixture.mutate(
                "/shell",
                {
                    "command": "python3 -c 'print(\"H\" * 32768)'",
                    "shell": "bash",
                    "exclude_from_context": False,
                },
            )
        fixture.terminal_start()
        assert fixture.terminal is not None and fixture.host is not None and fixture.rpc is not None
        fixture.terminal.read(10)
        pids = [
            fixture.host.pid,
            *descendants(fixture.rpc.process.pid),
            *descendants(fixture.terminal.process.pid),
        ]
        pids = sorted(set(pids))
        names = {pid: Path(f"/proc/{pid}/comm").read_text().strip() for pid in pids}
        events_before = fixture.call("/tui/snapshot")["events"][-1]["sequence"]
        before = {pid: ticks(pid) for pid in pids}
        started = time.monotonic()
        fixture.terminal.read(5)
        elapsed = time.monotonic() - started
        cpu = {
            f"{names[pid]}:{pid}": (ticks(pid) - before[pid])
            / os.sysconf("SC_CLK_TCK")
            / elapsed
            * 100
            for pid in pids
        }
        snapshot = fixture.call("/tui/snapshot")
        new_events = [event for event in snapshot["events"] if event["sequence"] > events_before]
        latency = []
        for index in range(20):
            marker = f"idle-latency-{index:02}"
            fixture.terminal.send(b"\x15")
            start = time.monotonic()
            os.write(fixture.terminal.master, marker.encode())
            until = start + 5
            while marker not in fixture.terminal.display:
                assert time.monotonic() < until, "input was not painted"
                fixture.terminal.read(0.005)
            latency.append((time.monotonic() - start) * 1000)
        fixture.terminal.send(b"\x15")
        started = time.monotonic()
        sync = fixture.call(
            f"/tui/snapshot?history_head={snapshot['history'][-1]['sequence']}&events_after={snapshot['events'][-1]['sequence']}&incremental=1"
        )
        sync_cost = {
            "elapsed_ms": (time.monotonic() - started) * 1000,
            "bytes": len(json.dumps(sync).encode()),
        }
        polls = []
        for _ in range(4):
            query = f"/tui/snapshot?after={snapshot['presentation']['sequence']}&events_after={snapshot['events'][-1]['sequence']}&history_head={snapshot['history'][-1]['sequence']}&incremental=1"
            started = time.monotonic()
            result = fixture.call(query)
            polls.append(
                {
                    "elapsed_ms": (time.monotonic() - started) * 1000,
                    "bytes": len(json.dumps(result).encode()),
                    "events": len(result["events"]),
                    "history_unchanged": result["history_unchanged"],
                }
            )
            if result["history_unchanged"]:
                result["history"] = snapshot["history"]
            if not result["events"]:
                result["events"] = snapshot["events"]
            snapshot = result
        report = {
            "history_records": len(snapshot["history"]),
            "history_bytes": len(json.dumps(snapshot["history"]).encode()),
            "cpu_single_core_percent": cpu,
            "idle_seconds": elapsed,
            "idle_event_counts": dict(
                Counter(
                    f"{event['kind']}:{event['payload'].get('contract', '')}:{event['payload'].get('input', {}).get('operation', '')}"
                    for event in new_events
                )
            ),
            "idle_service_events": sum(event["kind"] == "service_called" for event in new_events),
            "input_to_paint_ms": {
                "median": statistics.median(latency),
                "p95": sorted(latency)[18],
                "max": max(latency),
            },
            "incremental_polls": polls,
            "immediate_incremental_sync": sync_cost,
        }
        (args.output / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
        if args.assert_idle:
            assert report["idle_service_events"] <= 2, (
                "idle snapshots repeatedly read the store and wake themselves"
            )
            assert all(poll["elapsed_ms"] > 3500 for poll in polls[1:]), "idle long poll self-woke"


if __name__ == "__main__":
    main()
