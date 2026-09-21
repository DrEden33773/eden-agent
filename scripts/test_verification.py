"""Regressions for reusable preparation and failure evidence, without compiling Rust."""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

import verification
from install import Composition, Package


class VerificationTests(unittest.TestCase):
    def test_export_updates_changed_inputs_and_removes_deleted_files_without_retouching_unchanged(
        self,
    ):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            files = {pathlib.Path("sdk/Cargo.toml"): b"original", pathlib.Path("old.rs"): b"old"}
            verification.sync_files(files, root)
            original_time = (root / "sdk/Cargo.toml").stat().st_mtime_ns
            verification.sync_files(files, root)
            self.assertEqual(original_time, (root / "sdk/Cargo.toml").stat().st_mtime_ns)
            verification.sync_files({pathlib.Path("sdk/Cargo.toml"): b"changed"}, root)
            self.assertEqual(b"changed", (root / "sdk/Cargo.toml").read_bytes())
            self.assertFalse((root / "old.rs").exists())

    def test_source_fingerprint_detects_worktree_lock_flags_and_deleted_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "Cargo.lock").write_text("locked-v1")
            (root / "source.rs").write_text("original")
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            before = verification.source_fingerprint(root)
            (root / "Cargo.lock").write_text("locked-v2")
            changed_lock = verification.source_fingerprint(root)
            self.assertNotEqual(before, changed_lock)
            (root / "source.rs").unlink()
            deleted_source = verification.source_fingerprint(root)
            self.assertNotEqual(changed_lock, deleted_source)
            with patch.dict(os.environ, {"RUSTFLAGS": "--cfg verification_regression"}):
                self.assertNotEqual(deleted_source, verification.source_fingerprint(root))

    def test_prepared_host_and_each_feature_are_bound_to_receipt(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "host").write_bytes(b"host-fixed-before-authors")
            (root / "wrong-abi").write_bytes(b"abi-artifact")
            (root / "wrong-sdk").write_bytes(b"sdk-artifact")
            receipt = {
                "source_fingerprint": "source",
                "frozen": {str(root): verification.tree_hashes(root)},
            }
            verification.validate_receipt(receipt, "source")
            with self.assertRaisesRegex(RuntimeError, "source changed"):
                verification.validate_receipt(receipt, "new-lock-or-source")
            rewritten = (root / "wrong-abi").stat()
            (root / "wrong-abi").write_bytes((root / "wrong-sdk").read_bytes())
            # The memo keys on size and mtime, so the rewrite must move the clock
            # forward even where the filesystem timestamp is coarse.
            os.utime(
                root / "wrong-abi", ns=(rewritten.st_atime_ns, rewritten.st_mtime_ns + 1_000_000)
            )
            with self.assertRaisesRegex(RuntimeError, "artifacts changed"):
                verification.validate_receipt(receipt, "source")

    def test_output_cleanup_rejects_unowned_data_and_replaces_only_owned_results(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "user-data").write_text("keep")
            with self.assertRaisesRegex(ValueError, "not owned"):
                verification.reset_output(root)
            self.assertEqual("keep", (root / "user-data").read_text())
            output = root / "results"
            verification.reset_output(output)
            (output / "old-failure.log").write_text("old")
            verification.reset_output(output)
            self.assertFalse((output / "old-failure.log").exists())
            self.assertTrue((output / verification.OUTPUT_MARKER).is_file())
            with self.assertRaisesRegex(ValueError, "unsafe"):
                verification.reset_output(verification.ROOT)

    def test_installations_are_fresh_independent_copies(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            seed = root / "seeds/default"
            seed.mkdir(parents=True)
            (seed / "composition.json").write_text("original")
            with patch.object(verification, "prepare", return_value={"seeds": str(root / "seeds")}):
                first = verification.installed(root / "first")
                second = verification.installed(root / "second")
                (first / "composition.json").write_text("mutation")
                (first / "stale-session").write_text("stale")
                self.assertEqual("original", (second / "composition.json").read_text())
                self.assertEqual("original", (seed / "composition.json").read_text())
                verification.installed(first)
                self.assertFalse((first / "stale-session").exists())
                self.assertEqual("original", (first / "composition.json").read_text())

    def test_repeated_tree_hashes_reuse_contents_but_follow_rewrites(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / "artifact").write_bytes(b"first")
            before = verification.tree_hashes(root)
            self.assertEqual({"artifact": verification.digest(root / "artifact")}, before)
            self.assertEqual(before, verification.tree_hashes(root))
            # Same size, new mtime: the memo may not hide the rewrite.
            written = (root / "artifact").stat()
            (root / "artifact").write_bytes(b"other")
            os.utime(root / "artifact", ns=(written.st_atime_ns, written.st_mtime_ns + 1_000_000))
            self.assertNotEqual(before, verification.tree_hashes(root))

    def test_probe_binaries_live_outside_the_installation_seeds(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with patch.object(verification, "prepare", return_value={"examples": str(root)}):
                suffix = ".exe" if os.name == "nt" else ""
                self.assertEqual(
                    root / ("coding_probe" + suffix), verification.example("coding_probe")
                )

    def test_retry_delay_injection_touches_only_the_coding_package(self):
        def entry(name: str, config: dict[str, Any] | None) -> Package:
            return {
                "descriptor": {"package": name, "version": "0.1.0", "provides": []},
                "host": "eden-native-0.3.0",
                "sdk": "eden-native-0.3.0",
                "target": "x86_64-unknown-linux-gnu",
                "library": "lib" + name + ".so",
                "config": config,
            }

        composition: Composition = {
            "packages": [
                entry("coding", None),
                entry("coding", {"compaction": {"keep_recent_tokens": 1}}),
                entry("standard", None),
            ],
            "roles": {},
        }
        verification.short_retries(composition)
        self.assertEqual({"retry": {"base_delay_ms": 1}}, composition["packages"][0]["config"])
        self.assertEqual(
            {"compaction": {"keep_recent_tokens": 1}, "retry": {"base_delay_ms": 1}},
            composition["packages"][1]["config"],
        )
        self.assertIsNone(composition["packages"][2]["config"])

    def test_failure_preserves_command_streams_and_exit_code(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with patch.dict(os.environ, {"EDEN_VERIFICATION_LOG": temp}):
                with self.assertRaisesRegex(RuntimeError, "exit 7") as error:
                    verification.run(
                        [
                            sys.executable,
                            "-c",
                            "import sys; print('out'); print('err', file=sys.stderr); sys.exit(7)",
                        ],
                        root,
                    )
            self.assertIn("out", str(error.exception))
            self.assertIn("err", str(error.exception))
            record = json.loads(next(root.glob("*.json")).read_text())
            self.assertEqual(7, record["exit_code"])
            self.assertEqual("failed", record["status"])
            self.assertEqual("out\n", (root / record["stdout"]).read_text())
            self.assertEqual("err\n", (root / record["stderr"]).read_text())

    def test_timeout_keeps_partial_output_and_identifies_command(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with patch.dict(os.environ, {"EDEN_VERIFICATION_LOG": temp}):
                with self.assertRaisesRegex(RuntimeError, "timeout after") as error:
                    verification.run(
                        [
                            sys.executable,
                            "-u",
                            "-c",
                            "import time; print('before-block'); time.sleep(60)",
                        ],
                        root,
                        timeout=0.5,
                    )
            self.assertIn("before-block", str(error.exception))
            record = json.loads(next(root.glob("*.json")).read_text())
            self.assertEqual("timeout", record["status"])
            self.assertIn("before-block", (root / record["stdout"]).read_text())

    def test_expected_nonzero_retains_failure_evidence_for_caller_assertions(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with patch.dict(os.environ, {"EDEN_VERIFICATION_LOG": temp}):
                result = verification.run(
                    [sys.executable, "-c", "raise SystemExit(3)"], root, check=False
                )
            self.assertEqual(3, result.returncode)
            self.assertEqual(
                "nonzero-returned", json.loads(next(root.glob("*.json")).read_text())["status"]
            )


if __name__ == "__main__":
    unittest.main()
