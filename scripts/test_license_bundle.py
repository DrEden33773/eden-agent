"""Fast filesystem regressions for license validation; no Cargo build or network."""

import json
import pathlib
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

import license_bundle as licenses

ROOT = pathlib.Path(__file__).resolve().parents[1]


class LicenseBundleTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="eden-license-tests-")
        self.addCleanup(temporary.cleanup)
        self.root = pathlib.Path(temporary.name)

    def package(self, name: str, version: str = "1.0.0") -> dict[str, Any]:
        directory = self.root / "cache" / f"{name}-{version}"
        directory.mkdir(parents=True)
        manifest = directory / "Cargo.toml"
        manifest.write_text("", encoding="utf-8")
        return {
            "name": name,
            "version": version,
            "manifest_path": str(manifest),
            "source": licenses.REGISTRY,
            "license": "MIT",
            "license_file": None,
        }

    def test_unrelated_cache_ancestor_is_never_a_license_and_all_failures_are_reported(
        self,
    ) -> None:
        packages = [self.package("first"), self.package("second")]
        (self.root / "cache" / "LICENSE").write_text("unrelated cache owner", encoding="utf-8")
        destination = self.root / "distribution"
        destination.mkdir()
        with patch.object(licenses, "metadata", return_value={"packages": packages}):
            with self.assertRaises(RuntimeError) as error:
                licenses.bundle(self.root, destination, "test-target")
        self.assertIn("first 1.0.0: missing license", str(error.exception))
        self.assertIn("second 1.0.0: missing license", str(error.exception))
        self.assertFalse((destination / "third-party-licenses").exists())

    def test_case_insensitive_notices_and_declared_nested_license_are_copied(self) -> None:
        package = self.package("named")
        directory = pathlib.Path(package["manifest_path"]).parent
        (directory / "legal").mkdir()
        files = {"license-mit": "full license", "notice": "required notice", "legal/terms": "terms"}
        for relative, text in files.items():
            (directory / relative).write_text(text, encoding="utf-8")
        package["license_file"] = "legal/terms"
        with patch.object(licenses, "metadata", return_value={"packages": [package]}):
            licenses.bundle(self.root, self.root, "test-target")
        output = self.root / "third-party-licenses"
        for relative, text in files.items():
            self.assertEqual((output / "named-1.0.0" / relative).read_text(encoding="utf-8"), text)
        index = json.loads((output / "index.json").read_text(encoding="utf-8"))
        self.assertEqual(set(index[0]["files"]), {f"named-1.0.0/{path}" for path in files})

    def test_declared_license_cannot_be_missing_or_escape_even_when_another_license_exists(
        self,
    ) -> None:
        packages = [self.package("missing"), self.package("escape")]
        for package in packages:
            directory = pathlib.Path(package["manifest_path"]).parent
            (directory / "LICENSE").write_text("other license", encoding="utf-8")
        packages[0]["license_file"] = "legal/missing"
        packages[1]["license_file"] = "../LICENSE"
        (self.root / "cache" / "LICENSE").write_text("cache license", encoding="utf-8")
        with self.assertRaises(RuntimeError) as error:
            licenses.plan(self.root, packages)
        self.assertIn("missing 1.0.0: declared license_file is missing", str(error.exception))
        self.assertIn("escape 1.0.0: declared license_file escapes", str(error.exception))

    def test_objc_fallback_requires_exact_published_revision_and_retains_notice_and_full_license(
        self,
    ) -> None:
        packages = [
            self.package(name, "0.3.2") for name in ("objc2-core-foundation", "objc2-core-services")
        ]
        for package in packages:
            directory = pathlib.Path(package["manifest_path"]).parent
            (directory / ".cargo_vcs_info.json").write_text(
                json.dumps(
                    {
                        "git": {"sha1": licenses.OBJC_REVISION},
                        "path_in_vcs": f"framework-crates/{package['name']}",
                    }
                ),
                encoding="utf-8",
            )
        entries = licenses.plan(ROOT, packages)
        for entry in entries:
            self.assertEqual(
                {path.name for path in entry.files}, {"LICENSE", "objc2-frameworks-0.3.2-LICENSE"}
            )
            self.assertIn(licenses.OBJC_REVISION, entry.source)
            self.assertEqual(
                entry.files[pathlib.Path("LICENSE")].read_bytes(), (ROOT / "LICENSE").read_bytes()
            )
        directory = pathlib.Path(packages[0]["manifest_path"]).parent
        (directory / ".cargo_vcs_info.json").write_text(
            json.dumps({"git": {"sha1": "different"}}), encoding="utf-8"
        )
        packages[1]["source"] = "registry+https://unrelated.invalid/index"
        with self.assertRaises(RuntimeError) as error:
            licenses.plan(ROOT, packages)
        self.assertIn(
            "objc2-core-foundation 0.3.2: published revision differs", str(error.exception)
        )
        self.assertIn("objc2-core-services 0.3.2: missing license", str(error.exception))

    def test_native_notices_are_mandatory_and_retained_byte_for_byte(self) -> None:
        packages = []
        for name, native in licenses.NATIVE_FILES.items():
            package = self.package(name)
            packages.append(package)
            directory = pathlib.Path(package["manifest_path"]).parent
            (directory / "LICENSE").write_text("wrapper license", encoding="utf-8")
            for relative in native:
                path = directory / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(f"{name}: {relative}\r\nexact notice\n".encode())
        with patch.object(licenses, "metadata", return_value={"packages": packages}):
            licenses.bundle(self.root, self.root, "test-target")
        for package in packages:
            directory = pathlib.Path(package["manifest_path"]).parent
            folder = self.root / "third-party-licenses" / f"{package['name']}-1.0.0"
            for relative in licenses.NATIVE_FILES[package["name"]]:
                self.assertEqual(
                    (folder / "native" / relative).read_bytes(), (directory / relative).read_bytes()
                )
                (directory / relative).unlink()
        with self.assertRaises(RuntimeError) as error:
            licenses.plan(self.root, packages)
        for package in packages:
            self.assertIn(
                f"{package['name']} 1.0.0: missing vendored native notices", str(error.exception)
            )
        for native in licenses.NATIVE_FILES.values():
            for relative in native:
                self.assertIn(relative, str(error.exception))


if __name__ == "__main__":
    unittest.main()
