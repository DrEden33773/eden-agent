"""Launcher storage regression; process boundaries are observed without spawning a UI."""

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import launch


class LauncherTests(unittest.TestCase):
    def test_new_launches_save_in_same_canonical_project_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project with spaces"
            project.mkdir()
            alias = root / "alias"
            alias.symlink_to(project, target_is_directory=True)
            commands = []

            def start(command, **kwargs):
                commands.append(command)
                endpoint = Path(command[command.index("--endpoint") + 1])
                endpoint.write_text(json.dumps({"session_id": 1}))
                return Mock()

            for cwd in (project, alias):
                with (
                    patch.object(launch, "ROOT", root),
                    patch("sys.argv", ["launch.py", "--cwd", str(cwd)]),
                    patch.object(launch.subprocess, "Popen", side_effect=start),
                    patch.object(launch.subprocess, "call", return_value=0),
                    self.assertRaises(SystemExit) as exit_status,
                ):
                    launch.main()
                self.assertEqual(exit_status.exception.code, 0)
            histories = [Path(cmd[cmd.index("--session") + 1]) for cmd in commands]
            self.assertNotEqual(histories[0], histories[1])
            for history in histories:
                self.assertEqual(history.parent, project / ".eden/sessions")


if __name__ == "__main__":
    unittest.main()
