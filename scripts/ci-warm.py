"""Populate the native build graphs on trusted main without running test bodies."""

import importlib
import os

from verification import ROOT, prepare, run


def main() -> None:
    selection = importlib.import_module("ci-cache").test_selection(os.environ["CI_CACHE_OS"])
    run(["node", "scripts/checks.mjs", "clippy"], ROOT, timeout=None)
    targets = [arg for name in selection["packages"] for arg in ("-p", name)] or ["--workspace"]
    run(["cargo", "test", "--locked", "--no-run", *targets], ROOT, timeout=None)
    run(["node", "scripts/checks.mjs", "test", "--no-run"], ROOT, timeout=None)
    prepare()


if __name__ == "__main__":
    main()
