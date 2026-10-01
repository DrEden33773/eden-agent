#!/usr/bin/env python3
"""Deprecated developer alias for the checked-in frontend build."""

import runpy
from pathlib import Path

if __name__ == "__main__":
    runpy.run_path(
        str(Path(__file__).resolve().parents[2] / "scripts/build-frontend.py"), run_name="__main__"
    )
