"""Windows installation cleanup must observe process exit, not endpoint removal."""

import importlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

workflows = importlib.import_module("verify-product-workflows")


class ProductWorkflowCleanupTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "win32", "Windows executable mapping semantics")
    def test_waits_for_running_image_before_deleting_installation(self):
        with tempfile.TemporaryDirectory(prefix="eden-image-exit-") as temporary:
            binary = Path(temporary) / "probe.exe"
            shutil.copyfile(Path(os.environ["WINDIR"]) / "System32/ping.exe", binary)
            # A real image remains mapped while its bounded network probe runs.
            with subprocess.Popen(
                [str(binary), "-n", "4", "127.0.0.1"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ) as process:
                try:
                    self.assertIsNone(process.poll(), "probe exited before lock observation")
                    with self.assertRaises(PermissionError):
                        binary.unlink()
                    workflows.wait_installed_exit(binary)
                    self.assertIsNotNone(
                        process.poll(), "exit barrier returned before process exit"
                    )
                    binary.unlink()
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait()
