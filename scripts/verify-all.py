#!/usr/bin/env python3
"""Prepare once, execute every installed suite in isolation, retain all diagnostics."""

import argparse
import concurrent.futures
import json
import os
import pathlib
import shutil
import sys
import time
from typing import Any

from install import ROOT
from verification import (
    EXAMPLES,
    openssl,
    prepare,
    reset_output,
    run,
    source_fingerprint,
    validate_receipt,
)

SUITES = {
    "native": ("verify.py", "verification.json"),
    "coding": ("verify-coding.py", "coding-verification.json"),
    "sessions": ("verify-sessions.py", "session-verification.json"),
    "context": ("verify-context.py", "context-verification.json"),
    "workspace": ("verify-workspace.py", "workspace-verification.json"),
}


def execute(name: str, script: str, receipt_file: str, output: pathlib.Path) -> dict[str, Any]:
    directory = output / name
    directory.mkdir(parents=True, exist_ok=True)
    old_result = ROOT / "artifacts" / receipt_file
    old_result.unlink(missing_ok=True)
    environment = {**os.environ, "EDEN_VERIFICATION_LOG": str(directory / "commands")}
    start = time.monotonic()
    print(f"Starting {name}", flush=True)
    result = run(
        [sys.executable, ROOT / "scripts" / script],
        ROOT,
        check=False,
        env=environment,
        timeout=None,
    )
    (directory / "stdout.log").write_text(result.stdout, encoding="utf-8")
    (directory / "stderr.log").write_text(result.stderr, encoding="utf-8")
    record = {
        "suite": name,
        "seconds": time.monotonic() - start,
        "exit_code": result.returncode,
        "status": "passed" if result.returncode == 0 and old_result.is_file() else "failed",
    }
    if old_result.is_file():
        shutil.copy2(old_result, directory / receipt_file)
    (directory / "result.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    print(f"{name}: {record['status']} ({record['seconds']:.2f}s)", flush=True)
    if record["status"] != "passed":
        print(result.stdout + result.stderr, file=sys.stderr, flush=True)
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workers", type=int, choices=range(1, 6), default=2)
    parser.add_argument("--output", type=pathlib.Path, default=ROOT / "artifacts/verification")
    args = parser.parse_args()
    output = args.output.resolve()
    reset_output(output)
    start = time.monotonic()
    report: dict[str, Any] = {"status": "failed", "suites": []}
    try:
        openssl()
        receipt = prepare(output)
        report["preparation"] = {
            key: receipt[key]
            for key in ("commit", "dirty", "source_fingerprint", "phases", "seconds")
        }
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
            pending = [
                executor.submit(execute, name, script, artifact, output)
                for name, (script, artifact) in SUITES.items()
            ]
            for future in concurrent.futures.as_completed(pending):
                report["suites"].append(future.result())
        validate_receipt(receipt, source_fingerprint())
        if any(suite["status"] != "passed" for suite in report["suites"]):
            raise RuntimeError(
                "Installed acceptance failed; see per-suite command logs and result.json"
            )
        distribution = ROOT / "artifacts/distribution"
        if distribution.exists():
            shutil.rmtree(distribution)
        shutil.copytree(pathlib.Path(receipt["seeds"]) / "default", distribution)
        suffix = ".exe" if os.name == "nt" else ""
        for example in EXAMPLES:
            (distribution / "bin" / (example + suffix)).unlink()
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        report["seconds"] = time.monotonic() - start
        (output / "timings.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
