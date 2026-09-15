"""Build cache identity and one-time legacy migration regressions."""

import importlib
import pathlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch

cache = importlib.import_module("ci-cache")
ENV = {"CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "1", "CARGO_PROFILE_TEST_DEBUG": "1"}
COMMIT = "a" * 40
FILES = {
    "Cargo.toml": "[workspace]\nmembers = []\n[profile.dev]\ndebug = 1\n",
    "Cargo.lock": "version = 4\n",
    "rust-toolchain.toml": '[toolchain]\nchannel = "1.98.1"\n',
    ".cargo/config.toml": "[build]\njobs = 2\n",
}


class CacheKeyTests(unittest.TestCase):
    def keys(self, files=None, environment=None, **kwargs):
        values = {
            "runner_os": "ubuntu-24.04",
            "architecture": "X64",
            "image": "20260907.300.1",
            "commit": COMMIT,
        }
        values.update(kwargs)
        return cache.cache_keys(files or FILES, environment or ENV, **values)

    def test_workflow_and_source_changes_do_not_change_build_compatibility(self) -> None:
        with tempfile.TemporaryDirectory(prefix="eden-ci-cache-") as temp:
            root = pathlib.Path(temp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            for name, text in FILES.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            workflow = root / ".github/workflows/quality.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text("original workflow", encoding="utf-8")
            source = root / "source.rs"
            source.write_text("original Rust source", encoding="utf-8")
            before = self.keys(cache.read_inputs(root))
            workflow.write_text("different logging and step ordering", encoding="utf-8")
            source.write_text("changed source must still be checked by Cargo", encoding="utf-8")
            self.assertEqual(before, self.keys(cache.read_inputs(root)))
            later = self.keys(cache.read_inputs(root), commit="b" * 40)
            self.assertNotEqual(before["key"], later["key"])
            self.assertEqual(before["restore_keys"], later["restore_keys"])
            nested = root / "new-package/Cargo.toml"
            nested.parent.mkdir()
            nested.write_text('[package]\nname = "new-package"\n', encoding="utf-8")
            self.assertNotEqual(
                before["build_inputs"], self.keys(cache.read_inputs(root))["build_inputs"]
            )

    def test_environment_changes_keep_separate_contexts(self) -> None:
        before = self.keys()
        changed_profile = dict(
            FILES, **{"Cargo.toml": FILES["Cargo.toml"].replace("debug = 1", "debug = 2")}
        )
        changed_toolchain = dict(
            FILES,
            **{"rust-toolchain.toml": FILES["rust-toolchain.toml"].replace("1.98.1", "1.99.0")},
        )
        changed_config = dict(
            FILES, **{".cargo/config.toml": '[build]\nrustflags = ["-Ctarget-cpu=native"]\n'}
        )
        for changed in (
            self.keys(changed_profile),
            self.keys(changed_toolchain),
            self.keys(changed_config),
            self.keys(environment={**ENV, "CARGO_PROFILE_DEV_DEBUG": "2"}),
            self.keys(environment={**ENV, "CC": "different-c-compiler"}),
            self.keys(image="20261001.1"),
            self.keys(runner_os="ubuntu-26.04"),
            self.keys(architecture="ARM64"),
        ):
            self.assertNotEqual(before["restore_keys"][1], changed["restore_keys"][1])
            self.assertIsNone(changed["legacy_candidate"])
        self.assertEqual(before, self.keys(environment={**ENV, "GITHUB_RUN_ID": "next-run"}))

    def test_lock_changes_reuse_only_the_same_environment_context(self) -> None:
        before = self.keys()
        changed = self.keys(
            dict(FILES, **{"Cargo.lock": "version = 4\n# changed dependency graph\n"})
        )
        self.assertNotEqual(before["build_inputs"], changed["build_inputs"])
        self.assertNotEqual(before["restore_keys"][0], changed["restore_keys"][0])
        self.assertEqual(before["restore_keys"][1], changed["restore_keys"][1])
        self.assertIsNone(changed["legacy_candidate"])
        self.assertEqual(len(changed["restore_keys"]), 2)

    def test_legacy_migration_requires_exact_inputs_platform_and_recorded_image(self) -> None:
        fingerprint = cache.identities(FILES, ENV)[1]
        with patch.object(cache, "LEGACY_BUILD_INPUTS", fingerprint):
            for (runner_os, architecture), (image, old_hash) in cache.LEGACY_IMAGES.items():
                selected = self.keys(runner_os=runner_os, architecture=architecture, image=image)
                expected = f"cargo-target-v1-{runner_os}-{architecture}-{old_hash}-{cache.LEGACY_CACHE_COMMIT}"
                self.assertEqual(selected["restore_keys"][-1], expected)
                self.assertEqual(selected["legacy_candidate"], expected)
                self.assertEqual(selected["legacy_source_commit"], cache.LEGACY_SOURCE_COMMIT)
                changed = self.keys(
                    runner_os=runner_os, architecture=architecture, image=image + "0"
                )
                self.assertIsNone(changed["legacy_candidate"])
            self.assertIsNone(
                self.keys(dict(FILES, **{"Cargo.lock": "version = 3\n"}))["legacy_candidate"]
            )
            self.assertIsNone(
                self.keys(environment={**ENV, "CARGO_INCREMENTAL": "1"})["legacy_candidate"]
            )
            self.assertIsNone(self.keys(runner_os="unrecorded-platform")["legacy_candidate"])
            self.assertIsNone(self.keys(architecture="unknown-architecture")["legacy_candidate"])

    def test_checkout_line_endings_preserve_toml_identity(self) -> None:
        crlf = {name: text.replace("\n", "\r\n") for name, text in FILES.items()}
        self.assertEqual(self.keys(), self.keys(crlf))

    def test_missing_image_or_inexact_revision_cannot_enable_a_broad_restore(self) -> None:
        for changed in ({"image": ""}, {"commit": "main"}, {"runner_os": "../other"}):
            with self.assertRaises(ValueError):
                self.keys(**changed)


if __name__ == "__main__":
    unittest.main()
