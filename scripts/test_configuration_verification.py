"""Explicit acceptance paths and failure diagnostics must survive environment differences."""

import importlib
import io
import json
import pathlib
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr
from unittest.mock import patch

forms = importlib.import_module("verify-configuration-forms")
runner = importlib.import_module("verify-all")


class ConfigurationVerificationTests(unittest.TestCase):
    def test_headless_is_selected_even_when_browser_dependencies_exist(self):
        with (
            patch.object(forms.shutil, "which", return_value="agent-browser"),
            patch.object(pathlib.Path, "is_file", return_value=True),
        ):
            self.assertEqual(forms.frontend_reason("headless"), "headless selected explicitly")
            self.assertIsNone(forms.frontend_reason("browser"))
            self.assertIsNone(forms.frontend_reason("auto"))

    def test_explicit_browser_does_not_silently_fall_back(self):
        for executable, built, message in [
            (None, True, "agent-browser is not installed"),
            ("agent-browser", False, "Web renderer dist is not built"),
        ]:
            with (
                self.subTest(message=message),
                patch.object(forms.shutil, "which", return_value=executable),
                patch.object(pathlib.Path, "is_file", return_value=built),
            ):
                self.assertEqual(forms.frontend_reason("auto"), message)
                with self.assertRaisesRegex(RuntimeError, message):
                    forms.frontend_reason("browser")

    def test_private_leak_still_fails_after_sanitized_logs_are_saved(self):
        with tempfile.TemporaryDirectory() as temp:
            output = pathlib.Path(temp)
            logs = {"tui.ansi": "pending D2_SET_CANARY", "host.stderr.log": "reply rejected"}
            with self.assertRaisesRegex(AssertionError, "Private material"):
                forms.write_diagnostics(output, logs)
            self.assertEqual((output / "tui.ansi").read_text(), "pending [private input redacted]")
            self.assertEqual((output / "host.stderr.log").read_text(), "reply rejected")

    def test_cleanup_exception_chain_is_redacted_before_stderr_capture(self):
        def fail():
            try:
                raise AssertionError(b"terminal exited with D2_SET_CANARY")
            except AssertionError:
                raise AssertionError("Private material escaped into public output") from None

        def fail_with_context():
            try:
                raise AssertionError(b"terminal exited with D2_REPLACE_CANARY")
            except AssertionError as error:
                raise RuntimeError("cleanup failed") from error

        for failure in (fail, fail_with_context):
            with self.subTest(failure=failure.__name__):
                stderr = io.StringIO()
                with patch.object(forms, "main", side_effect=failure), redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as result:
                        forms.cli()
                self.assertEqual(result.exception.code, 1)
                self.assertNotRegex(stderr.getvalue(), r"D2_[A-Z_]*CANARY")
                self.assertIn("AssertionError", stderr.getvalue())
        self.assertIn("cleanup failed", stderr.getvalue())
        self.assertIn("[private input redacted]", stderr.getvalue())

    def test_failed_suite_keeps_details_inside_its_collected_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "artifacts").mkdir()

            def fail(command, cwd, *, env, **kwargs):
                details = pathlib.Path(env["EDEN_VERIFICATION_DIAGNOSTICS"])
                details.mkdir(parents=True)
                forms.write_diagnostics(details, {"tui.ansi": "StaleRevision"})
                return subprocess.CompletedProcess(command, 1, "", "refresh failed")

            with patch.object(runner, "ROOT", root), patch.object(runner, "run", side_effect=fail):
                result = runner.execute("configuration-forms", "fixture.py", "receipt.json", root)
            suite = root / "configuration-forms"
            self.assertEqual(result["status"], "failed")
            self.assertEqual((suite / "diagnostics/tui.ansi").read_text(), "StaleRevision")
            self.assertEqual(json.loads((suite / "result.json").read_text())["exit_code"], 1)


if __name__ == "__main__":
    unittest.main()
