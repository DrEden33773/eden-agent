#!/usr/bin/env python3
"""Create or restore an Eden host, then attach the isolated whole-pager experiment."""

import argparse
import json
import subprocess
import sys
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cwd", type=Path, default=Path.cwd())
    parser.add_argument("--resume", type=Path, help="Stopped Eden JSONL history")
    parser.add_argument("--host", type=Path, default=ROOT / "artifacts/r1-candidate/bin/eden")
    args = parser.parse_args()
    task = ROOT / "artifacts/g1-grok-port/sessions" / str(uuid.uuid4())
    task.mkdir(parents=True)
    endpoint = task / "endpoint.json"
    history = args.resume.resolve() if args.resume else task / "history.jsonl"
    command = [
        str(args.host.resolve()),
        "--cwd",
        str(args.cwd.resolve()),
        "--session",
        str(history),
        "--offline-startup",
        "resume-live" if args.resume else "live",
        "--endpoint",
        str(endpoint),
    ]
    with (task / "host.log").open("wb") as log:
        host = subprocess.Popen(
            command, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True
        )
    deadline = time.monotonic() + 30
    while not endpoint.exists():
        if host.poll() is not None or time.monotonic() > deadline:
            raise SystemExit(f"Host did not start; see {task / 'host.log'}")
        time.sleep(0.05)
    json.loads(endpoint.read_text())
    result = subprocess.call(
        [sys.executable, str(HERE / "run.py"), "--endpoint", str(endpoint)], cwd=args.cwd
    )
    print(f"Attach: python3 {HERE / 'run.py'} --endpoint {endpoint}")
    print(f"History: {history}")
    print(f"Stop host: attach with {args.host} live-tui --endpoint {endpoint}, then /stop-host.")
    raise SystemExit(result)


if __name__ == "__main__":
    main()
