"""Resolve the fixed frontend protobuf compiler at build time only."""

import hashlib
import io
import json
import os
import platform
import shutil
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def protoc():
    """Use an explicit compiler or the reference's checksum-pinned distribution."""
    selected = os.environ.get("PROTOC") or shutil.which("protoc")
    if selected:
        return Path(selected).resolve()
    system = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}[platform.system()]
    architecture = {"arm64": "aarch64", "AMD64": "x86_64"}.get(
        platform.machine(), platform.machine()
    )
    reference = (ROOT / "frontend/grok/bin/protoc").read_text()
    distributions = json.loads(reference[reference.index("{") :])["platforms"]
    distributions["windows-x86_64"] = {
        "size": 3188624,
        "digest": "57ea59e9f551ad8d71ffaa9b5cfbe0ca1f4e720972a1db7ec2d12ab44bff9383",
        "providers": [
            {
                "url": "https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-win64.zip"
            }
        ],
    }
    key = f"{system}-{architecture}"
    if key not in distributions:
        raise RuntimeError(f"Set PROTOC to a protobuf compiler on {key}")
    distribution = distributions[key]
    directory = ROOT / "target/frontend-tools" / distribution["digest"]
    binary = directory / "bin" / ("protoc.exe" if os.name == "nt" else "protoc")
    if not binary.exists():
        with urllib.request.urlopen(distribution["providers"][0]["url"], timeout=90) as response:
            archive = response.read()
        if (
            len(archive) != distribution["size"]
            or hashlib.sha256(archive).hexdigest() != distribution["digest"]
        ):
            raise RuntimeError("Pinned protobuf compiler archive differs from the fixed reference")
        with zipfile.ZipFile(io.BytesIO(archive)) as zip_file:
            for member in zip_file.namelist():
                path = Path(member)
                if ".." in path.parts or path.is_absolute():
                    raise RuntimeError("Invalid protobuf archive path")
                zip_file.extract(member, directory)
        binary.chmod(0o755)
    return binary
