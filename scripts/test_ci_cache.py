"""Build cache identity, one rolling key per context, and legacy migration."""

import importlib
import pathlib
import re
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


def workflow(name: str) -> str:
    return (cache.ROOT / ".github/workflows" / name).read_text(encoding="utf-8")


def block_after(text: str, marker: str) -> list[str]:
    """The lines of the step or mapping the marker belongs to, after the marker."""
    lines = text.splitlines()
    index = next(index for index, line in enumerate(lines) if marker in line)
    indent = len(lines[index]) - len(lines[index].lstrip())
    block = []
    for line in lines[index + 1 :]:
        if line.strip() and len(line) - len(line.lstrip()) < indent:
            break
        block.append(line)
    return block


def cache_paths(text: str, marker: str) -> list[str]:
    """The cargo-target path list of the step or mapping the marker introduces."""
    block = block_after(text, marker)
    for index, line in enumerate(block):
        if line.strip() != "path: |":
            continue
        indent = len(line) - len(line.lstrip())
        entries = []
        for following in block[index + 1 :]:
            if following.strip() and len(following) - len(following.lstrip()) <= indent:
                break
            if following.strip():
                entries.append(following.strip())
        return entries
    raise AssertionError(f"no path block follows {marker!r}")


def step_condition(text: str, marker: str) -> str:
    """The `if:` condition of the workflow step containing the marker."""
    lines = text.splitlines()
    index = next(index for index, line in enumerate(lines) if marker in line)
    start = max(index for index in range(index + 1) if re.match(r"^\s*- ", lines[index]))
    for line in lines[start:index]:
        match = re.match(r"^\s*if: (.+)$", line)
        if match:
            return match.group(1)
    raise AssertionError(f"no condition on the step containing {marker!r}")


def matrix_oses(text: str) -> set[str]:
    """The runner labels of the single runner matrix the workflow declares."""
    matches = re.findall(r"^\s+os: \[([^\]]*)\]$", text, re.MULTILINE)
    if len(matches) != 1:
        raise AssertionError(f"expected exactly one runner matrix, found {len(matches)}")
    return {label.strip() for label in matches[0].split(",")}


def declared_build_environment(text: str) -> dict[str, str]:
    """The literal build environment ci-cache.py folds into the identity."""
    declared = {}
    for line in text.splitlines():
        match = re.match(r"^\s+([A-Za-z0-9_]+): (.+?)\s*$", line)
        if not match:
            continue
        name = match.group(1)
        if name in cache.BUILD_ENV or name.startswith("CARGO_PROFILE_"):
            declared[name] = match.group(2)
    return declared


class CacheKeyCase(unittest.TestCase):
    def keys(self, files=None, environment=None, **kwargs):
        values = {
            "runner_os": "ubuntu-24.04",
            "architecture": "X64",
            "image": "20260907.300.1",
            "commit": COMMIT,
        }
        values.update(kwargs)
        return cache.cache_keys(files or FILES, environment or ENV, **values)


class CacheKeyTests(CacheKeyCase):
    def test_workflow_and_source_changes_do_not_change_build_compatibility(self) -> None:
        with tempfile.TemporaryDirectory(prefix="eden-ci-cache-") as temp:
            root = pathlib.Path(temp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            for name, text in FILES.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            workflow_file = root / ".github/workflows/quality.yml"
            workflow_file.parent.mkdir(parents=True)
            workflow_file.write_text("original workflow", encoding="utf-8")
            source = root / "source.rs"
            source.write_text("original Rust source", encoding="utf-8")
            before = self.keys(cache.read_inputs(root))
            workflow_file.write_text("different logging and step ordering", encoding="utf-8")
            source.write_text("changed source must still be checked by Cargo", encoding="utf-8")
            self.assertEqual(before, self.keys(cache.read_inputs(root)))
            nested = root / "new-package/Cargo.toml"
            nested.parent.mkdir()
            nested.write_text('[package]\nname = "new-package"\n', encoding="utf-8")
            self.assertNotEqual(
                before["build_inputs"], self.keys(cache.read_inputs(root))["build_inputs"]
            )

    def test_the_key_names_one_reusable_build_context_not_one_commit(self) -> None:
        before = self.keys()
        later = self.keys(commit="b" * 40)
        self.assertTrue(before["key"].startswith("cargo-target-v4-"))
        self.assertNotIn(COMMIT, before["key"])
        # One entry per compatible build context is what makes a later revision
        # restore it instead of writing an entry only its own revision can use.
        self.assertEqual(before["key"], later["key"])
        self.assertEqual(before["restore_keys"], later["restore_keys"])
        self.assertEqual(before["restore_keys"], [before["key"].rsplit("-", 1)[0] + "-"])
        # The exact commit is still required and still recorded, just not in the key.
        self.assertEqual(before["commit"], COMMIT)
        self.assertEqual(later["commit"], "b" * 40)

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
            self.keys(runner_os="windows-2022"),
            self.keys(architecture="ARM64"),
        ):
            self.assertNotEqual(before["restore_keys"][0], changed["restore_keys"][0])
            self.assertNotEqual(before["key"], changed["key"])
            self.assertIsNone(changed["legacy_candidate"])
        self.assertEqual(before, self.keys(environment={**ENV, "GITHUB_RUN_ID": "next-run"}))
        # Two runners that declare the same test selection still keep separate
        # entries: cache scope is per runner, and the label stays in the key.
        self.assertNotEqual(
            self.keys(runner_os="windows-2022")["restore_keys"],
            self.keys(runner_os="macos-14")["restore_keys"],
        )

    def test_lock_changes_reuse_only_the_same_environment_context(self) -> None:
        before = self.keys()
        changed = self.keys(
            dict(FILES, **{"Cargo.lock": "version = 4\n# changed dependency graph\n"})
        )
        self.assertNotEqual(before["build_inputs"], changed["build_inputs"])
        self.assertNotEqual(before["key"], changed["key"])
        # A lock change reuses the newest entry of the same environment context.
        self.assertEqual(before["restore_keys"], changed["restore_keys"])
        self.assertIsNone(changed["legacy_candidate"])
        self.assertEqual(len(changed["restore_keys"]), 1)

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
            self.assertIsNone(self.keys(architecture="unknown-architecture")["legacy_candidate"])

    def test_checkout_line_endings_preserve_toml_identity(self) -> None:
        crlf = {name: text.replace("\n", "\r\n") for name, text in FILES.items()}
        self.assertEqual(self.keys(), self.keys(crlf))

    def test_missing_image_or_inexact_revision_cannot_enable_a_broad_restore(self) -> None:
        for changed in (
            {"image": ""},
            {"commit": "main"},
            {"runner_os": "../other"},
            # An undeclared runner cannot name its unit graph, so it fails here
            # instead of keying one selection's entry as another's.
            {"runner_os": "ubuntu-26.04"},
        ):
            with self.assertRaises(ValueError):
                self.keys(**changed)


class SelectionIdentityTests(CacheKeyCase):
    """The test selection decides the unit graph, so it decides the key."""

    def test_the_declared_selection_names_the_phase_and_its_packages(self) -> None:
        workspace = cache.test_selection("ubuntu-24.04")
        platform = cache.test_selection("windows-2022")
        self.assertEqual(workspace["label"], "workspace-suites")
        self.assertEqual(workspace["packages"], [])
        self.assertEqual(platform["label"], "platform-suites")
        self.assertEqual(platform["packages"], sorted(cache.PLATFORM_PACKAGES))
        self.assertEqual(platform, cache.test_selection("macos-14"))
        self.assertNotEqual(workspace["identity"], platform["identity"])
        self.assertRegex(platform["identity"], r"^[a-f0-9]{64}$")
        with self.assertRaises(ValueError):
            cache.test_selection("ubuntu-26.04")

    def test_changing_the_selection_rekeys_the_build_context(self) -> None:
        before = self.keys()
        narrowed = {
            **cache.TEST_SELECTIONS,
            "ubuntu-24.04": ("platform-suites", cache.PLATFORM_PACKAGES),
        }
        with patch.object(cache, "TEST_SELECTIONS", narrowed):
            after = self.keys()
        # Nothing but the selection changed, so nothing but the selection may
        # explain the new key.
        self.assertEqual(before["os"], after["os"])
        self.assertEqual(before["build_inputs"], after["build_inputs"])
        self.assertEqual(before["build_environment"], after["build_environment"])
        self.assertEqual(before["test_selection"]["label"], "workspace-suites")
        self.assertEqual(after["test_selection"]["label"], "platform-suites")
        self.assertNotEqual(before["context"], after["context"])
        self.assertNotEqual(before["key"], after["key"])
        # The narrowed unit graph must be written under its own restore scope: a
        # run may not restore the other selection's entry as its newest context.
        self.assertNotEqual(before["restore_keys"], after["restore_keys"])
        self.assertNotIn(before["restore_keys"][0], after["restore_keys"])

    def test_moving_one_package_rekeys_the_build_context(self) -> None:
        before = self.keys(runner_os="windows-2022")
        one_package_less = {
            **cache.TEST_SELECTIONS,
            "windows-2022": ("platform-suites", cache.PLATFORM_PACKAGES[:-1]),
        }
        with patch.object(cache, "TEST_SELECTIONS", one_package_less):
            after = self.keys(runner_os="windows-2022")
        # The selection belongs to the context, not to the input fingerprint: the
        # manifests are the same, the unit graph is not.
        self.assertEqual(before["build_inputs"], after["build_inputs"])
        self.assertNotEqual(before["test_selection"], after["test_selection"])
        self.assertNotEqual(before["key"], after["key"])
        self.assertNotEqual(before["restore_keys"], after["restore_keys"])

    def test_an_unchanged_selection_keeps_the_key(self) -> None:
        forward = self.keys(runner_os="windows-2022")
        reordered = {
            **cache.TEST_SELECTIONS,
            "windows-2022": ("platform-suites", tuple(reversed(cache.PLATFORM_PACKAGES))),
        }
        with patch.object(cache, "TEST_SELECTIONS", reordered):
            backward = self.keys(runner_os="windows-2022")
        # Cargo resolves the same selection from reordered flags, so the entry is
        # still the one a reordered command restores.
        self.assertEqual(forward["test_selection"], backward["test_selection"])
        self.assertEqual(forward, backward)

    def test_the_workspace_selection_declares_no_packages(self) -> None:
        self.assertEqual(cache.TEST_SELECTIONS["ubuntu-24.04"], ("workspace-suites", ()))
        self.assertEqual(self.keys()["test_selection"]["packages"], [])


class WorkflowIdentityTests(unittest.TestCase):
    """The heavy job, the probe and the decision must name the same identity."""

    def setUp(self) -> None:
        self.heavy = workflow("native-verify.yml")
        self.probe = workflow("quality.yml")
        self.scope = (cache.ROOT / "scripts/ci-scope.mjs").read_text(encoding="utf-8")

    def test_every_declared_selection_is_the_command_the_workflow_runs(self) -> None:
        declared = {label: set(packages) for label, packages in cache.TEST_SELECTIONS.values()}
        commands = re.findall(r"ci-run\.mjs ([a-z0-9-]+) cargo test ([^\n]*)", self.heavy)
        # A new test phase must declare its selection, or its unit graph would be
        # stored under a key that ignores what it ran.
        self.assertEqual({label for label, _ in commands}, set(declared))
        for label, arguments in commands:
            self.assertEqual(set(re.findall(r"-p ([A-Za-z0-9_-]+)", arguments)), declared[label])
            self.assertEqual("--workspace" in arguments, not declared[label])

    def test_each_runner_declares_the_phase_its_condition_runs(self) -> None:
        conditions = {
            label: step_condition(self.heavy, f"ci-run.mjs {label} cargo test")
            for label, _ in cache.TEST_SELECTIONS.values()
        }
        self.assertIn("runner.os == 'Linux'", conditions["workspace-suites"])
        self.assertIn("runner.os != 'Linux'", conditions["platform-suites"])
        for runner, (label, _) in cache.TEST_SELECTIONS.items():
            expected = "workspace-suites" if runner.startswith("ubuntu") else "platform-suites"
            self.assertEqual(label, expected, runner)

    def test_the_cache_state_probe_looks_for_the_heavy_job_entry(self) -> None:
        # Same runners, so a runner cannot be probed with a selection it would
        # never run.
        self.assertEqual(matrix_oses(self.heavy), matrix_oses(self.probe))
        self.assertEqual(set(cache.TEST_SELECTIONS), matrix_oses(self.heavy))
        # Same identity inputs and the same literal build environment, so both
        # derivations of the key agree.
        self.assertEqual(
            re.findall(r"^\s+(CI_CACHE_[A-Z_]+): (.+)$", self.heavy, re.MULTILINE),
            re.findall(r"^\s+(CI_CACHE_[A-Z_]+): (.+)$", self.probe, re.MULTILINE),
        )
        self.assertEqual(
            declared_build_environment(self.heavy), declared_build_environment(self.probe)
        )
        # The path list is part of the cache version, so the probe's lookup and
        # the heavy job's save must list the same paths byte for byte.
        restored = cache_paths(self.heavy, "id: cargo-target")
        self.assertEqual(cache_paths(self.probe, "id: cargo-target"), restored)
        self.assertEqual(
            cache_paths(self.heavy, "steps.cargo-target.outcome == 'success'"), restored
        )

    def test_the_heavy_decision_covers_exactly_the_probed_runners(self) -> None:
        match = re.search(r"export const HEAVY_OSES = \[([^\]]*)\]", self.scope)
        declared = set(re.findall(r'"([^"]+)"', match.group(1))) if match else set()
        self.assertEqual(declared, matrix_oses(self.probe))


if __name__ == "__main__":
    unittest.main()
