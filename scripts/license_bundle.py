"""Preserve dependency license texts in the assembled distribution."""
import json
import pathlib
import shutil
import subprocess


def bundle(root, destination, triple):
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", triple], cwd=root, text=True))
    output = destination / "third-party-licenses"
    output.mkdir(exist_ok=True)
    index = []
    for package in metadata["packages"]:
        if package["source"] is None:
            continue
        directory = pathlib.Path(package["manifest_path"]).parent
        files = []
        for candidate in [directory, *list(directory.parents)[:3]]:
            files = sorted({path for pattern in ["LICENSE*", "LICENCE*", "COPYING*", "NOTICE*"] for path in candidate.glob(pattern) if path.is_file()})
            if files:
                break
        license_source = package["source"]
        if not files and (package["name"], package["version"], package["source"]) == (
            "cordis-core", "0.1.3", "registry+https://github.com/rust-lang/crates.io-index"
        ):
            # This published crate omitted LICENSE; preserve the exact upstream revision's text.
            files = [root / "third-party" / "cordis-core-0.1.3-LICENSE"]
            license_source = "https://github.com/dshbox/cordis-rs/blob/fba0506191bfbc678d3646f338e42a94e0f6057b/LICENSE"
        if not files:
            raise RuntimeError(f"Missing license text for {package['name']} {package['version']}")
        folder = output / f"{package['name']}-{package['version']}"
        folder.mkdir(exist_ok=True)
        for path in files:
            shutil.copy2(path, folder / path.name)
        index.append({"package": package["name"], "version": package["version"], "license": package["license"], "source": package["source"], "license_source": license_source, "files": [f"{folder.name}/{path.name}" for path in files]})
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
