#!/usr/bin/env python3
"""Compatibility alias for real installed native-pager recovery acceptance."""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

if __name__ == "__main__":
    raise SystemExit(
        subprocess.call(
            [sys.executable, str(ROOT / "scripts/verify-session-recovery.py"), *sys.argv[1:]],
            cwd=ROOT,
        )
    )
