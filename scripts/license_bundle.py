"""Validate and preserve exact dependency license texts without compiling code."""

import argparse
import json
import pathlib
import shutil
import subprocess
from dataclasses import dataclass
from typing import Any

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
PLATFORMS = ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc")
OBJC_REVISION = "7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"
NATIVE_FILES = {
    "lmdb-master-sys": ("lmdb/libraries/liblmdb/LICENSE", "lmdb/libraries/liblmdb/COPYRIGHT"),
    "libgit2-sys": ("libgit2/COPYING", "libgit2/AUTHORS"),
    "libz-sys": ("src/zlib/LICENSE",),
}


@dataclass(frozen=True)
class Fallback:
    files: tuple[str, ...]
    source: str
    revision: str | None = None


FALLBACKS = {
    ("cordis-core", "0.2.10"): Fallback(
        ("third-party/cordis-core-0.2.10-LICENSE",),
        "https://github.com/dshbox/cordis-rs/blob/ba4babd73996b0c8821128e5a561e407ae2682b7/LICENSE",
    ),
    **dict.fromkeys(
        ((name, "0.10.6") for name in ("fff-search", "fff-grep", "fff-query-parser")),
        Fallback(
            ("third-party/fff-0.10.6-LICENSE",),
            "https://github.com/dmtrKovalenko/fff/blob/c6013ba6a5918221b6c482486aca01acc0830825/LICENSE",
        ),
    ),
    ("glidesort", "0.1.2"): Fallback(
        # This exact README offers Apache-2.0 but the crate omits its full text.
        ("LICENSE",),
        "https://github.com/orlp/glidesort/blob/c4df9518ec23a424a867b3d50967f0aaafe3c0fd/README.md#license",
    ),
    **{
        (name, version): Fallback(
            (f"third-party/{name}-{version}-LICENSE",),
            f"https://github.com/meilisearch/heed/blob/{revision}/LICENSE",
        )
        for name, version, revision in (
            ("heed", "0.22.1", "57025ff083b6fc68c9313edccb28c75d7ccf751f"),
            ("heed-traits", "0.20.0", "9de8469341cafcd5f239317fff85db955103fcd9"),
            ("heed-types", "0.21.0", "178c53261595880bb89554bc7f1b55dcda45ae50"),
        )
    },
    ("lmdb-master-sys", "0.2.6"): Fallback(
        ("LICENSE",),
        "https://github.com/meilisearch/heed/blob/57025ff083b6fc68c9313edccb28c75d7ccf751f/lmdb-master-sys/Cargo.toml",
    ),
    **dict.fromkeys(
        ((name, "0.3.2") for name in ("objc2-core-foundation", "objc2-core-services")),
        Fallback(
            # Retain the upstream SDK notice and the full offered Apache-2.0 text.
            ("third-party/objc2-frameworks-0.3.2-LICENSE", "LICENSE"),
            f"https://github.com/madsmtm/objc2/blob/{OBJC_REVISION}/LICENSE.md",
            OBJC_REVISION,
        ),
    ),
}


@dataclass
class PackageLicenses:
    package: dict[str, Any]
    source: str
    files: dict[pathlib.Path, pathlib.Path]
    native: tuple[str, ...]


def metadata(root: pathlib.Path, triple: str) -> dict[str, Any]:
    return json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", triple],
            cwd=root,
            text=True,
            encoding="utf-8",
        )
    )


def package_licenses(root: pathlib.Path, package: dict[str, Any]) -> PackageLicenses:
    directory = pathlib.Path(package["manifest_path"]).parent.resolve()
    files: dict[pathlib.Path, pathlib.Path] = {}
    has_license = False
    # Cargo cache ancestors contain unrelated crates; they never establish a package's license.
    for path in sorted(directory.iterdir()):
        name = path.name.lower()
        if path.is_file() and name.startswith(("license", "licence", "copying", "notice")):
            if not path.resolve().is_relative_to(directory):
                raise ValueError(f"license file escapes package directory: {path.name}")
            files[pathlib.Path(path.name)] = path
            has_license |= name.startswith(("license", "licence", "copying"))
    if declared := package.get("license_file"):
        path = (directory / declared).resolve()
        if not path.is_relative_to(directory):
            raise ValueError(f"declared license_file escapes package directory: {declared}")
        if not path.is_file():
            raise ValueError(f"declared license_file is missing: {declared}")
        files[path.relative_to(directory)] = path
        has_license = True
    source = package["source"]
    if not has_license:
        fallback = FALLBACKS.get((package["name"], package["version"]))
        if fallback is None or source != REGISTRY:
            raise ValueError("missing license text in published package")
        if fallback.revision:
            vcs = json.loads((directory / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
            if vcs.get("git", {}).get("sha1") != fallback.revision or vcs.get("path_in_vcs") != (
                f"framework-crates/{package['name']}"
            ):
                raise ValueError("published revision differs from the recorded license source")
        for relative in fallback.files:
            path = root / relative
            if not path.is_file():
                raise ValueError(f"missing pinned fallback text: {relative}")
            files[pathlib.Path(path.name)] = path
        source = fallback.source
    native = NATIVE_FILES.get(package["name"], ())
    missing = [relative for relative in native if not (directory / relative).is_file()]
    if missing:
        raise ValueError(f"missing vendored native notices: {', '.join(missing)}")
    for relative in native:
        files[pathlib.Path("native") / relative] = directory / relative
    return PackageLicenses(package, source, files, native)


def plan(root: pathlib.Path, packages: list[dict[str, Any]]) -> list[PackageLicenses]:
    entries = []
    failures = []
    for package in packages:
        if package["source"] is None:
            continue
        try:
            entries.append(package_licenses(root, package))
        except (OSError, ValueError) as error:
            failures.append(f"{package['name']} {package['version']}: {error}")
    if failures:
        raise RuntimeError("Dependency license validation failed:\n" + "\n".join(failures))
    return entries


def bundle(root: pathlib.Path, destination: pathlib.Path, triple: str) -> None:
    # Validate every package before writing a partial license bundle.
    entries = plan(root, metadata(root, triple)["packages"])
    output = destination / "third-party-licenses"
    output.mkdir(exist_ok=True)
    index = []
    for entry in entries:
        package = entry.package
        folder = output / f"{package['name']}-{package['version']}"
        folder.mkdir(exist_ok=True)
        for relative, path in entry.files.items():
            target = folder / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
        index.append(
            {
                "package": package["name"],
                "version": package["version"],
                "license": package["license"],
                "source": package["source"],
                "license_source": entry.source,
                "native_license_files": list(entry.native),
                "files": [f"{folder.name}/{path.as_posix()}" for path in entry.files],
            }
        )
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check", action="store_true", required=True, help="validate without copying"
    )
    targets = parser.add_mutually_exclusive_group(required=True)
    targets.add_argument(
        "--all-platforms", action="store_true", help="check all three release targets"
    )
    targets.add_argument("--target", help="check one Rust target triple")
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    failures = []
    results = {}
    for triple in PLATFORMS if args.all_platforms else (args.target,):
        try:
            entries = plan(root, metadata(root, triple)["packages"])
            results[triple] = {
                "packages": len(entries),
                "files": sum(len(e.files) for e in entries),
            }
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            failures.append(f"{triple}: {error}")
    if failures:
        raise SystemExit("\n".join(failures))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
