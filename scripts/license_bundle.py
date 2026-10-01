"""Validate and preserve exact dependency license texts without compiling code."""

import argparse
import json
import pathlib
import re
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


FRONTEND_GIT_SOURCES = {
    "git+https://github.com/our-forks/async-openai.git?rev=95b52ebdedf42143083cf3d6f0e0be7c84e9c808#95b52ebdedf42143083cf3d6f0e0be7c84e9c808",
    "git+https://github.com/helix-editor/nucleo.git?rev=5b74652#5b74652e482f7c07d827f18c6d21e7540c242c69",
}

FALLBACKS = {
    ("debug_unsafe", "0.1.3"): Fallback(
        ("third-party/debug_unsafe-0.1.3-LICENSE",),
        "https://github.com/RoDmitry/debug_unsafe/blob/a06246d06e6cec80e7140e48b706811b87978958/LICENSE-MIT",
    ),
    ("rhai_codegen", "3.2.0"): Fallback(
        ("third-party/rhai_codegen-3.2.0-LICENSE",),
        "https://github.com/rhaiscript/rhai/blob/b247dd730de3f2c97fa1e46f138a17f9d844de67/LICENSE-MIT.txt",
    ),
    ("taffy", "0.12.2"): Fallback(
        ("third-party/taffy-0.12.2-LICENSE",),
        "https://github.com/DioxusLabs/taffy/blob/bb351fcc056c93dbb967292acc4242a5d1c46b3c/LICENSE",
    ),
    **dict.fromkeys(
        ((name, "3.4.0") for name in ("rmcp", "rmcp-macros")),
        Fallback(
            ("third-party/rmcp-3.4.0-LICENSE",),
            "https://github.com/modelcontextprotocol/rust-sdk/blob/fd7811fdaa9fefa1c8034534b4d7a31c97204f89/LICENSE",
        ),
    ),
    **{
        (name, version): Fallback(
            ("third-party/google-cloud-rust-LICENSE",),
            f"https://github.com/googleapis/google-cloud-rust/blob/{revision}/LICENSE",
        )
        for name, version, revision in (
            ("google-cloud-auth", "1.16.0", "4ae1bfd6813b4687f8af5494a6795b07a12ba341"),
            ("google-cloud-gax", "1.14.0", "4ae1bfd6813b4687f8af5494a6795b07a12ba341"),
            ("google-cloud-rpc", "1.6.0", "544a5d8f29117f2611aeadc4b3d30843710371e2"),
            ("google-cloud-wkt", "1.7.0", "25e0da468d47057a9ea244f039f8d79d5228d2f1"),
        )
    },
    **dict.fromkeys(
        ((name, "0.8.0") for name in ("base64-simd", "uuid-simd", "vsimd")),
        Fallback(
            ("third-party/simd-0.8.0-LICENSE",),
            "https://github.com/Nugine/simd/blob/d74c030d9dc4f3cae02146d1f497ff62726ef09a/LICENSE",
        ),
    ),
    **dict.fromkeys(
        ((name, "0.56.0") for name in ("jsonschema-regex", "jsonschema-value")),
        Fallback(
            ("third-party/jsonschema-0.56.0-LICENSE",),
            "https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/LICENSE",
        ),
    ),
    ("defmt-parser", "1.0.0"): Fallback(
        ("third-party/defmt-parser-1.0.0-LICENSE",),
        "https://github.com/knurling-rs/defmt/blob/4a8cdb44891ed57b8ff5a023b6bec7137c48708f/LICENSE-MIT",
    ),
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
            # The fixed frontend source includes complete per-package notices and shared
            # license texts for packages whose published archives omit them. Match both
            # version and declared license; an unrelated cache ancestor is never a fallback.
            notice = root / "frontend/grok/THIRD-PARTY-NOTICES"
            marker = f"\n{package['name']} {package['version']}\n"
            text = notice.read_text(encoding="utf-8") if notice.is_file() else ""
            section = (
                text.split(marker, 1)[-1].split(
                    "\n--------------------------------------------------------------------------------\n",
                    1,
                )[0]
                if marker in text
                else ""
            )
            license_line = re.search(r"^License: (.+)$", section, re.MULTILINE)
            declared = (
                re.search(r"upstream declares: ([^)]+)", license_line[1]) if license_line else None
            )
            expected = (
                declared[1] if declared else license_line[1].strip() if license_line else None
            )
            omitted = "missing license text" in str(
                error
            ) or "license file escapes package directory" in str(error)
            if (
                omitted
                and package["source"] in {REGISTRY, *FRONTEND_GIT_SOURCES}
                and expected == package.get("license")
            ):
                entries.append(
                    PackageLicenses(
                        package,
                        f"Grok Build {(root / 'frontend/grok/SOURCE_REV').read_text().strip()} bundled notices",
                        {pathlib.Path("GROK-THIRD-PARTY-NOTICES"): notice},
                        (),
                    )
                )
            else:
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
