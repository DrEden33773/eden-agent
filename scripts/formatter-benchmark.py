"""Compare a baseline formatter and the prepared tool; stdout is a small JSON receipt."""

import ctypes
import json
import os
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any, cast


def sample(command: list[str], source: bytes) -> dict[str, Any]:
    started = time.perf_counter()
    child = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    )
    output, error = child.communicate(source)
    seconds = time.perf_counter() - started
    if child.returncode:
        raise RuntimeError(error.decode(errors="replace"))
    if sys.platform == "win32":
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            cb: int
            peak: int
            _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
                (name, ctypes.c_size_t)
                for name in (
                    "peak",
                    "working",
                    "peak_paged",
                    "paged",
                    "peak_nonpaged",
                    "nonpaged",
                    "pagefile",
                    "peak_pagefile",
                )
            ]

        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        get_memory = ctypes.WinDLL("psapi", use_last_error=True).GetProcessMemoryInfo
        get_memory.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
        get_memory.restype = wintypes.BOOL
        if not get_memory(int(cast(Any, child)._handle), ctypes.byref(counters), counters.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        peak = counters.peak
        method = "Windows formatter process PeakWorkingSetSize"
    else:
        import resource

        peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        if sys.platform != "darwin":
            peak *= 1024
        method = "POSIX maximum child-process RSS (not aggregate tree RSS)"
    return {
        "seconds": seconds,
        "peak_rss_bytes": peak,
        "memory_method": method,
        "output": output.decode(),
    }


def main() -> None:
    if sys.argv[1:2] == ["sample"]:
        print(json.dumps(sample(sys.argv[2:], sys.stdin.buffer.read())))
        return
    root = pathlib.Path.cwd()
    current = subprocess.check_output(["node", "scripts/formatter-tool.mjs"], text=True).strip()
    receipt: dict[str, Any] = {
        "platform": sys.platform,
        "cpu_budget": subprocess.check_output(
            ["node", "-p", "require('node:os').availableParallelism()"], text=True
        ).strip(),
        "rows": [],
    }
    with tempfile.TemporaryDirectory(prefix="eden-fmt-baseline-") as temporary:
        baseline = pathlib.Path(temporary)
        archive = subprocess.check_output(["git", "archive", "77bd79a"])
        import io
        import tarfile

        with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
            bundle.extractall(baseline, filter="data")
        subprocess.run(
            [
                "cargo",
                "build",
                "--release",
                "--locked",
                "-p",
                "eden-fmt",
                "--target-dir",
                str(baseline / "target"),
            ],
            cwd=baseline,
            check=True,
            stdout=sys.stderr,
        )
        old = str(baseline / "target/release" / ("eden-fmt.exe" if os.name == "nt" else "eden-fmt"))
        inputs = {
            "tiny": b'fn main() { println!("hello"); }\n',
            "dirty": b'fn f(){let x=serde_json::json!({"a":[1,2,3]});}\n',
        }
        inputs.update(
            {
                name: (root / path).read_bytes()
                for name, path in [
                    ("scope", "crates/eden-plugin-sdk/src/scope.rs"),
                    ("kernel", "crates/eden-kernel/src/lib.rs"),
                    ("search", "plugins/search/src/engine.rs"),
                ]
            }
        )
        inputs["search-dirty-macro"] = (
            inputs["search"] + b'\nfn probe(){let _x=serde_json::json!({"a":[1,2,3]});}\n'
        )
        for name, source in inputs.items():
            outputs = []
            for mode, command in [
                ("old-release", [old, "stdin"]),
                ("new-release", [current, "stdin"]),
                ("editor", ["node", "scripts/format-editor.mjs"]),
            ]:
                samples = [
                    json.loads(
                        subprocess.check_output(
                            [sys.executable, __file__, "sample", *command], input=source
                        )
                    )
                    for _ in range(4)
                ]
                outputs.append(samples[-1]["output"])
                receipt["rows"].append(
                    {
                        "input": name,
                        "mode": mode,
                        "median_seconds": statistics.median(
                            item["seconds"] for item in samples[1:]
                        ),
                        "peak_rss_bytes": max(item["peak_rss_bytes"] for item in samples[1:]),
                        "memory_method": samples[-1]["memory_method"],
                    }
                )
            if len(set(outputs)) != 1:
                raise AssertionError(f"different output for {name}")
        for mode, binary, jobs in [
            ("old-1", old, "1"),
            ("new-1", current, "1"),
            ("new-4", current, "4"),
            ("new-auto", current, None),
        ]:
            command = [binary, "check", "crates/eden-plugin-sdk/src", "crates/eden-kernel/src"] + (
                ["--jobs", jobs] if jobs else []
            )
            samples = [
                json.loads(
                    subprocess.check_output(
                        [sys.executable, __file__, "sample", *command], input=b""
                    )
                )
                for _ in range(4)
            ]
            receipt["rows"].append(
                {
                    "input": "batch-12",
                    "tasks": 12,
                    "mode": mode,
                    "requested_jobs": jobs,
                    "effective_workers": min(int(jobs or receipt["cpu_budget"]), 12),
                    "median_seconds": statistics.median(item["seconds"] for item in samples[1:]),
                    "peak_rss_bytes": max(item["peak_rss_bytes"] for item in samples[1:]),
                    "memory_method": samples[-1]["memory_method"],
                }
            )
    receipt["target_inventory_bytes"] = {
        child.name: sum(path.stat().st_size for path in child.rglob("*") if path.is_file())
        for child in (root / "target").iterdir()
        if child.is_dir()
    }
    output = root / "artifacts/ci/formatter-performance.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
