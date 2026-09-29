#!/usr/bin/env python3
"""Prepare once, execute every installed suite in isolation, retain all diagnostics."""

import argparse
import concurrent.futures
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
from typing import Any

from install import ROOT
from verification import (
    openssl,
    prepare,
    reset_output,
    run,
    source_fingerprint,
    validate_receipt,
)

SUITES = {
    "tui": ("verify-tui.py", "tui-verification.json"),
    "configuration-forms": (
        "verify-configuration-forms.py",
        "configuration-forms-verification.json",
    ),
    "notes": ("verify-notes.py", "notes-verification.json"),
    "cache-warmer": ("verify-cache-warmer.py", "cache-warmer-verification.json"),
    "delivery": ("verify-delivery.py", "delivery-verification.json"),
    "native": ("verify.py", "verification.json"),
    "presentation": ("verify-presentation.py", "presentation-verification.json"),
    "coding": ("verify-coding.py", "coding-verification.json"),
    "sessions": ("verify-sessions.py", "session-verification.json"),
    "context": ("verify-context.py", "context-verification.json"),
    "shared-context": ("verify-shared-context.py", "shared-context-verification.json"),
    "shared-context-tui": ("verify-shared-context-tui.py", "shared-context-tui-verification.json"),
    "workspace": ("verify-workspace.py", "workspace-verification.json"),
    "models": ("verify-model-access.py", "model-access-verification.json"),
    "cloud-models": ("verify-cloud-model-access.py", "cloud-model-access-verification.json"),
    "router-models": ("verify-router-models.py", "router-models-verification.json"),
    "oauth-models": ("verify-oauth-models.py", "oauth-models-verification.json"),
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


def cpu_budget() -> int:
    explicit = os.environ.get("EDEN_CPU_BUDGET")
    if explicit is not None:
        if not explicit.isdecimal() or int(explicit) < 1:
            raise ValueError("EDEN_CPU_BUDGET must be a positive integer")
        return int(explicit)
    try:
        return max(
            1,
            int(
                subprocess.check_output(
                    ["node", "-p", "require('node:os').availableParallelism()"], text=True
                )
            ),
        )
    except (OSError, ValueError, subprocess.CalledProcessError):
        return max(1, os.cpu_count() or 1)


def suite_order() -> list[str]:
    # A2 observed durations; priorities start the release-build and other tails
    # early, without pretending that a scheduling thread reserves a CPU core.
    if sys.platform == "win32":
        return [
            "workspace",
            "context",
            "models",
            "coding",
            "sessions",
            "delivery",
            "router-models",
            "cloud-models",
            "oauth-models",
            "presentation",
            "tui",
            "configuration-forms",
            "notes",
            "cache-warmer",
            "native",
        ]
    if sys.platform == "darwin":
        return [
            "workspace",
            "presentation",
            "tui",
            "configuration-forms",
            "sessions",
            "models",
            "context",
            "coding",
            "router-models",
            "delivery",
            "cloud-models",
            "oauth-models",
            "notes",
            "cache-warmer",
            "native",
        ]
    return [
        "workspace",
        "presentation",
        "tui",
        "configuration-forms",
        "coding",
        "context",
        "sessions",
        "models",
        "delivery",
        "router-models",
        "cloud-models",
        "oauth-models",
        "notes",
        "cache-warmer",
        "native",
    ]


def choose_schedule(budget: int, requested: int | None, order: str) -> tuple[int, list[str]]:
    # Five suites helped the 16-CPU comparison, but not consistently the 3/4-CPU
    # hosted runners. Retain their established queue and four-suite ceiling.
    ceiling = 5 if budget > 4 else 4
    workers = min(requested or min(budget + 1, ceiling), len(SUITES))
    longest = order == "longest" or (order == "auto" and budget > 4)
    return workers, suite_order() if longest else list(SUITES)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workers", type=int, help="positive suite limit (capped by suite count)")
    parser.add_argument("--order", choices=("auto", "longest", "declared"), default="auto")
    parser.add_argument("--output", type=pathlib.Path, default=ROOT / "artifacts/verification")
    args = parser.parse_args()
    budget = cpu_budget()
    if args.workers is not None and args.workers < 1:
        parser.error("--workers must be positive")
    workers, order = choose_schedule(budget, args.workers, args.order)
    output = args.output.resolve()
    reset_output(output)
    start = time.monotonic()
    report: dict[str, Any] = {
        "status": "failed",
        "suites": [],
        "cpu_budget": budget,
        "requested_workers": args.workers,
        "workers": workers,
        "order": order,
    }
    try:
        openssl()
        receipt = prepare(output)
        report["preparation"] = {
            key: receipt[key]
            for key in ("commit", "dirty", "source_fingerprint", "phases", "compilation", "seconds")
        }
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
            pending = [executor.submit(execute, name, *SUITES[name], output) for name in order]
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
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        report["seconds"] = time.monotonic() - start
        (output / "timings.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
