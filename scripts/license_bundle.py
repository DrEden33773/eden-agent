"""Preserve dependency license texts in the assembled distribution."""

import json
import pathlib
import shutil
import subprocess


def bundle(root: pathlib.Path, destination: pathlib.Path, triple: str) -> None:
    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                triple,
            ],
            cwd=root,
            text=True,
        )
    )
    output = destination / "third-party-licenses"
    output.mkdir(exist_ok=True)
    index = []
    for package in metadata["packages"]:
        if package["source"] is None:
            continue
        directory = pathlib.Path(package["manifest_path"]).parent
        files = []
        for candidate in [directory, *list(directory.parents)[:3]]:
            files = sorted(
                {
                    path
                    for pattern in ["LICENSE*", "LICENCE*", "COPYING*", "NOTICE*"]
                    for path in candidate.glob(pattern)
                    if path.is_file()
                }
            )
            if files:
                break
        license_source = package["source"]
        if not files and (package["name"], package["version"], package["source"]) == (
            "cordis-core",
            "0.1.3",
            "registry+https://github.com/rust-lang/crates.io-index",
        ):
            # This published crate omitted LICENSE; preserve the exact upstream revision's text.
            files = [root / "third-party" / "cordis-core-0.1.3-LICENSE"]
            license_source = "https://github.com/dshbox/cordis-rs/blob/fba0506191bfbc678d3646f338e42a94e0f6057b/LICENSE"
        if (
            not files
            and package["name"] in {"fff-search", "fff-grep", "fff-query-parser"}
            and package["version"] == "0.10.6"
            and package["source"] == "registry+https://github.com/rust-lang/crates.io-index"
        ):
            # These registry crates omitted the workspace-root MIT license.
            files = [root / "third-party" / "fff-0.10.6-LICENSE"]
            license_source = "https://github.com/dmtrKovalenko/fff/blob/c6013ba6a5918221b6c482486aca01acc0830825/LICENSE"
        if not files and (package["name"], package["version"], package["source"]) == (
            "glidesort",
            "0.1.2",
            "registry+https://github.com/rust-lang/crates.io-index",
        ):
            # The exact published README grants MIT OR Apache-2.0 but omits
            # both license files. Redistribute under Apache-2.0 and include its
            # full standard text, also used by this project.
            files = [root / "LICENSE"]
            license_source = "https://github.com/orlp/glidesort/blob/c4df9518ec23a424a867b3d50967f0aaafe3c0fd/README.md#license"
        heed_revisions = {
            ("heed", "0.22.1"): "57025ff083b6fc68c9313edccb28c75d7ccf751f",
            ("heed-traits", "0.20.0"): "9de8469341cafcd5f239317fff85db955103fcd9",
            ("heed-types", "0.21.0"): "178c53261595880bb89554bc7f1b55dcda45ae50",
        }
        revision = heed_revisions.get((package["name"], package["version"]))
        if (
            not files
            and revision
            and package["source"] == "registry+https://github.com/rust-lang/crates.io-index"
        ):
            files = [root / "third-party" / f"{package['name']}-{package['version']}-LICENSE"]
            license_source = f"https://github.com/meilisearch/heed/blob/{revision}/LICENSE"
        if not files and package["name"] == "lmdb-master-sys" and package["version"] == "0.2.6":
            files = [root / "LICENSE"]
            license_source = "https://github.com/meilisearch/heed/blob/57025ff083b6fc68c9313edccb28c75d7ccf751f/lmdb-master-sys/Cargo.toml"
        if not files:
            raise RuntimeError(f"Missing license text for {package['name']} {package['version']}")
        folder = output / f"{package['name']}-{package['version']}"
        folder.mkdir(exist_ok=True)
        for path in files:
            shutil.copy2(path, folder / path.name)
        native_files = {
            "lmdb-master-sys": [
                "lmdb/libraries/liblmdb/LICENSE",
                "lmdb/libraries/liblmdb/COPYRIGHT",
            ],
            "libgit2-sys": ["libgit2/COPYING", "libgit2/AUTHORS"],
            "libz-sys": ["src/zlib/LICENSE"],
        }.get(package["name"], [])
        for relative in native_files:
            native = folder / "native" / relative
            native.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(directory / relative, native)
        index.append(
            {
                "package": package["name"],
                "version": package["version"],
                "license": package["license"],
                "source": package["source"],
                "license_source": license_source,
                "native_license_files": native_files,
                "files": [f"{folder.name}/{path.name}" for path in files],
            }
        )
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
