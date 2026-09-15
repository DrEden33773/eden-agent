"""Archive verification must reject missing/corrupted files before CLI execution."""

import importlib
import os
import pathlib
import tarfile
import tempfile
import unittest

archive = importlib.import_module("verify-archive")


class ArchiveTests(unittest.TestCase):
    def test_packaged_tree_is_exact_and_keeps_the_unrelated_parent_receipt(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            source = root / "source"
            (source / "bin").mkdir(parents=True)
            (source / "bin/eden").write_bytes(b"fixture executable")
            (source / "bin/eden").chmod(0o755)
            (source / "composition.json").write_bytes(b"fixture composition")
            expected = archive.file_state(source)
            bundle = root / "installation.tar.gz"
            with tarfile.open(bundle, "w:gz") as output:
                output.add(source, arcname=".")
            sentinel = root / "receipt.json"
            sentinel.write_bytes(b"unrelated parent")
            archive.extract_verified(bundle, root / "unpacked", expected)
            self.assertEqual(sentinel.read_bytes(), b"unrelated parent")
            self.assertEqual(archive.file_state(root / "unpacked"), expected)

    def test_missing_corrupted_and_extra_files_cannot_pass(self):
        mutations = ("missing", "corrupted", "extra")
        if os.name != "nt":
            mutations += ("permissions",)
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temp:
                root = pathlib.Path(temp)
                source = root / "source"
                source.mkdir()
                library = source / "library"
                library.write_bytes(b"original native bytes")
                library.chmod(0o755)
                expected = archive.file_state(source)
                if mutation == "missing":
                    library.unlink()
                elif mutation == "corrupted":
                    library.write_bytes(b"changed native bytes")
                elif mutation == "extra":
                    (source / "unverified-library").write_bytes(b"extra")
                else:
                    library.chmod(0o644)
                bundle = root / "installation.tar.gz"
                with tarfile.open(bundle, "w:gz") as output:
                    output.add(source, arcname=".")
                with self.assertRaisesRegex(ValueError, "Archive"):
                    archive.extract_verified(bundle, root / "unpacked", expected)


if __name__ == "__main__":
    unittest.main()
