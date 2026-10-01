"""Compatibility argv checks; native recovery is verified by the installed PTY driver."""

import unittest
from pathlib import Path
from unittest.mock import patch

import launch


class Delegation(unittest.TestCase):
    def test_resume_executes_formal_cli_with_explicit_history(self):
        host = Path("/tmp/fixture-install/bin/eden").resolve()
        with (
            patch(
                "sys.argv",
                [
                    "launch.py",
                    "--host",
                    str(host),
                    "--cwd",
                    "/tmp/project",
                    "--resume",
                    "/tmp/history.jsonl",
                ],
            ),
            patch.object(launch.os, "execv") as execute,
        ):
            launch.main()
        execute.assert_called_once_with(
            str(host), [str(host), "--cwd", "/tmp/project", "--session", "/tmp/history.jsonl"]
        )

    def test_endpoint_executes_formal_cli_attachment(self):
        host = Path("/tmp/fixture-install/bin/eden").resolve()
        with (
            patch(
                "sys.argv", ["launch.py", "--host", str(host), "--endpoint", "/tmp/endpoint.json"]
            ),
            patch.object(launch.os, "execv") as execute,
        ):
            launch.main()
        execute.assert_called_once_with(
            str(host), [str(host), "tui", "--endpoint", "/tmp/endpoint.json"]
        )


if __name__ == "__main__":
    unittest.main()
