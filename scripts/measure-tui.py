#!/usr/bin/env python3
"""Measure process cold-open and input-to-PTY output on isolated full histories.

Build release eden and eden-terminal-editor first. No model, clipboard or real
account is used. Files are freshly generated; filesystem caches are not flushed.
"""

import argparse
import importlib.util
import json
import os
import platform
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

from install import ROOT, library
from tui_pty import Terminal


def peak_kib(pid: int) -> int | None:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1])
    except (OSError, ValueError):
        return None
    return None


def observe(terminal: Terminal, text: str, deadline: float) -> None:
    while text not in terminal.display:
        terminal.read(0.002)
        if terminal.process.poll() is not None or time.monotonic() > deadline:
            raise AssertionError(f"Expected {text!r}:\n{terminal.display}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/eden")
    parser.add_argument(
        "--editor", type=Path, default=ROOT / "target/release" / library("eden_terminal_editor")
    )
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/tui-performance.json")
    parser.add_argument("--records", type=int, nargs="+", default=[1000, 10000, 100000])
    parser.add_argument("--no-color", action="store_true")
    parser.add_argument(
        "--search", action="store_true", help="navigate to the middle of the loaded history"
    )
    args = parser.parse_args()
    if any(count < 2 for count in args.records):
        parser.error("record counts must be at least two")
    if os.name == "nt":
        raise SystemExit("This probe needs a POSIX PTY; use native state tests on Windows")
    binary, editor = args.binary.resolve(), args.editor.resolve()
    if not binary.is_file() or not editor.is_file():
        parser.error("build release eden-cli and eden-terminal-editor first")
    spec = importlib.util.spec_from_file_location(
        "performance_live", ROOT / "scripts/verify-tui.py"
    )
    assert spec and spec.loader
    live = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(live)
    results: dict[str, Any] = {
        "platform": platform.platform(),
        "binary": str(binary),
        "editor": str(editor),
        "viewport": [120, 36],
        "no_color": args.no_color,
        "search": args.search,
        "samples": 20,
        "observation_poll_ms": 2,
        "filesystem_caches_flushed": False,
        "cases": [],
    }
    with tempfile.TemporaryDirectory(prefix="eden-tui-performance-") as temporary:
        root = Path(temporary)
        for count in args.records:
            history = root / f"history-{count}.jsonl"
            marker = f"PERF_LAST_{count}"
            with history.open("w", encoding="utf-8") as output:
                for index in range(count + 1):
                    record = {
                        "schema_version": 2,
                        "session_id": count,
                        "sequence": index + 1,
                        "run_id": 1 if index else 0,
                        "parent_id": index if index else None,
                        "branch": "main",
                        "kind": "message" if index else "session",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {
                                    "type": "text",
                                    "text": f"{marker if index == count else index}: 中文 é 🧭 full-history message for native terminal measurement.",
                                }
                            ],
                        }
                        if index
                        else {"cwd": str(root)},
                    }
                    output.write(
                        json.dumps(
                            {"schema_version": 2, "transaction": [record]}, ensure_ascii=False
                        )
                        + "\n"
                    )
            endpoint_file = root / f"endpoint-{count}.json"
            start = time.monotonic()
            host = subprocess.Popen(
                [str(binary), "read", str(history), "--endpoint", str(endpoint_file)],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            endpoint = live.ready(host, endpoint_file)
            host_ms = (time.monotonic() - start) * 1000
            terminal = None
            try:
                start = time.monotonic()
                terminal = Terminal(
                    binary,
                    endpoint_file,
                    env={
                        "EDEN_TUI_EDITOR": str(editor),
                        **({"NO_COLOR": "1"} if args.no_color else {}),
                    },
                    width=120,
                    height=36,
                )
                observe(terminal, marker, start + 45)
                first_ms = (time.monotonic() - start) * 1000
                first_bytes = len(terminal.output)
                samples = []
                for sample in range(20):
                    text = f"INPUT_PROBE_{sample:02}"
                    start = time.monotonic()
                    os.write(terminal.master, b"\x15" + text.encode())
                    observe(terminal, text, start + 5)
                    samples.append((time.monotonic() - start) * 1000)
                samples.sort()
                case = {
                    "messages": count,
                    "history_bytes": history.stat().st_size,
                    "host_start_ms": host_ms,
                    "tui_first_frame_ms": first_ms,
                    "input_p50_ms": samples[10],
                    "input_p95_ms": samples[18],
                    "input_max_ms": samples[19],
                    "first_frame_output_bytes": first_bytes,
                    "input_output_bytes": len(terminal.output) - first_bytes,
                    "host_peak_kib": peak_kib(host.pid),
                    "tui_peak_kib": peak_kib(terminal.process.pid),
                }
                if args.search:
                    terminal.send(b"\x15")
                    terminal.command("/search")
                    terminal.wait("Search by name, ID or category")
                    before_search = len(terminal.output)
                    start = time.monotonic()
                    os.write(terminal.master, f"{count // 2}:\r".encode())
                    middle = f"{count // 2}: 中文"
                    while (
                        "⌕" in terminal.display
                        or marker in terminal.display
                        or middle not in terminal.display
                    ):
                        terminal.read(0.002)
                        if time.monotonic() - start > 10:
                            raise AssertionError(
                                f"search did not navigate to the middle:\n{terminal.display}"
                            )
                    case["search_navigation_ms"] = (time.monotonic() - start) * 1000
                    case["search_output_bytes"] = len(terminal.output) - before_search
                    case["search_middle"] = True
                if args.no_color:
                    assert b"38;2;" not in terminal.output and b"48;2;" not in terminal.output
                results["cases"].append(case)
                print(json.dumps(case), flush=True)
            finally:
                if terminal is not None:
                    terminal.close()
                live.stop(host, endpoint)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
