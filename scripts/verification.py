"""Shared, measured preparation for isolated installed acceptance suites.

Only Cargo's compilation outputs are reused across runs. A preparation receipt is
valid for this source snapshot and these immutable installed bytes; it is never a
cached test verdict. SDK consumers are built after freezing the host seed.
"""

import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import time
from collections.abc import Sequence
from typing import Any

from install import ROOT, Composition, build_target, install, library, target

AUTHORS = (
    "loop-a",
    "context-b",
    "lifecycle",
    "init-probe",
    "coding-replacements",
    "service-a",
    "service-b",
)
INVALID = ("wrong-abi", "wrong-sdk", "short-table", "metadata-panic", "init-panic")
EXAMPLES = (
    "embedded",
    "contract_probe",
    "initialization_probe",
    "coding_probe",
    "session_probe",
    "context_probe",
    "workspace_probe",
)
_RECEIPT: dict[str, Any] | None = None


OUTPUT_MARKER = ".eden-verification-output"


def reset_output(output: pathlib.Path) -> None:
    """Only replace a directory this harness previously marked as diagnostic output."""
    resolved = output.resolve()
    if (
        output.is_symlink()
        or ROOT.is_relative_to(resolved)
        or build_target().is_relative_to(resolved)
        or resolved.is_relative_to(build_target())
    ):
        raise ValueError(f"Refusing unsafe verification output: {output}")
    marker = output / OUTPUT_MARKER
    if output.exists():
        if any(output.iterdir()) and (
            not marker.is_file() or marker.read_text(encoding="utf-8") != "eden-verification-v1\n"
        ):
            raise ValueError(
                f"Verification output is not owned by this harness: {output}; choose a new directory"
            )
        shutil.rmtree(output)
    output.mkdir(parents=True)
    marker.write_text("eden-verification-v1\n", encoding="utf-8")


def openssl() -> str:
    executable = shutil.which("openssl")
    if executable:
        return executable
    for path in (
        pathlib.Path("C:/Program Files/Git/usr/bin/openssl.exe"),
        pathlib.Path("C:/Program Files/Git/mingw64/bin/openssl.exe"),
    ):
        if path.is_file():
            return str(path)
    raise RuntimeError("Workspace TLS acceptance requires OpenSSL (including Git for Windows)")


# A frozen tree is validated by every process that inherits the receipt, so the
# same bytes would otherwise be hashed once per suite. Contents are read once and
# reused while the size and modification time are unchanged, which is what every
# writer in this harness changes. An in-place rewrite that keeps both is outside
# what this memo can see, and no frozen tree is written after it is frozen.
_DIGESTS: dict[tuple[str, int, int], str] = {}


def digest(path: pathlib.Path) -> str:
    stat = path.stat()
    key = (str(path), stat.st_size, stat.st_mtime_ns)
    known = _DIGESTS.get(key)
    if known is None:
        with path.open("rb") as stream:
            known = hashlib.file_digest(stream, "sha256").hexdigest()
        _DIGESTS[key] = known
    return known


def run(
    args: Sequence[str | pathlib.Path],
    cwd: pathlib.Path,
    check: bool = True,
    env: dict[str, str] | None = None,
    timeout: float | None = 240,
) -> subprocess.CompletedProcess[str]:
    """Keep the exact failed command, both streams and timing, including timeouts."""
    command = [str(arg) for arg in args]
    start = time.monotonic()
    status = "error"
    stdout = stderr = ""
    code: int | None = None
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
            timeout=timeout,
        )
        stdout, stderr, code = result.stdout, result.stderr, result.returncode
        status = "passed" if code == 0 else "failed" if check else "nonzero-returned"
        if check and code:
            raise RuntimeError(f"{command} (cwd={cwd}): exit {code}\n{stdout}\n{stderr}")
        return result
    except subprocess.TimeoutExpired as error:
        status = "timeout"
        stdout = (
            error.stdout.decode("utf-8", "replace")
            if isinstance(error.stdout, bytes)
            else error.stdout or ""
        )
        stderr = (
            error.stderr.decode("utf-8", "replace")
            if isinstance(error.stderr, bytes)
            else error.stderr or ""
        )
        raise RuntimeError(
            f"{command} (cwd={cwd}): timeout after {timeout}s\n{stdout}\n{stderr}"
        ) from error
    except OSError as error:
        stderr = str(error)
        raise RuntimeError(f"Cannot execute {command} (cwd={cwd}): {error}") from error
    finally:
        log_directory = os.environ.get("EDEN_VERIFICATION_LOG")
        if log_directory:
            folder = pathlib.Path(log_directory)
            folder.mkdir(parents=True, exist_ok=True)
            identity = f"{time.time_ns()}-{os.getpid()}"
            (folder / f"{identity}.stdout.log").write_text(stdout, encoding="utf-8")
            (folder / f"{identity}.stderr.log").write_text(stderr, encoding="utf-8")
            record = {
                "command": command,
                "cwd": str(cwd),
                "seconds": time.monotonic() - start,
                "status": status,
                "exit_code": code,
                "cargo_compiling_lines": len(re.findall(r"^\s*Compiling ", stderr, re.MULTILINE)),
                "cargo_checking_lines": len(re.findall(r"^\s*Checking ", stderr, re.MULTILINE)),
                "stdout": f"{identity}.stdout.log",
                "stderr": f"{identity}.stderr.log",
            }
            (folder / f"{identity}.json").write_text(
                json.dumps(record, indent=2) + "\n", encoding="utf-8"
            )


def source_fingerprint(root: pathlib.Path = ROOT) -> str:
    """Content, names, toolchain and build flags; no dependency on checkout mtimes."""
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root
    ).split(b"\0")
    fingerprint = hashlib.sha256()
    for name in sorted(set(names)):
        if not name:
            continue
        path = root / os.fsdecode(name)
        fingerprint.update(name + b"\0")
        if path.is_file():
            fingerprint.update(bytes.fromhex(digest(path)))
        else:
            fingerprint.update(b"missing")
    flags = {key: value for key, value in os.environ.items() if key.startswith(("CARGO_", "RUST"))}
    fingerprint.update(json.dumps(flags, sort_keys=True).encode())
    fingerprint.update(subprocess.check_output(["rustc", "-vV"], cwd=root))
    return fingerprint.hexdigest()


def sync_files(files: dict[pathlib.Path, bytes], destination: pathlib.Path) -> None:
    """Update changed content and remove stale inputs; preserve unchanged file mtimes."""
    destination.mkdir(parents=True, exist_ok=True)
    for path in destination.rglob("*"):
        if path.is_file() and path.relative_to(destination) not in files:
            path.unlink()
    for relative, contents in files.items():
        path = destination / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.is_file() or path.read_bytes() != contents:
            path.write_bytes(contents)


def export_authors(destination: pathlib.Path) -> None:
    files: dict[pathlib.Path, bytes] = {}
    for name in ("eden-protocol", "eden-plugin-sdk"):
        base = ROOT / "crates" / name
        for path in base.rglob("*"):
            if path.is_file() and "target" not in path.relative_to(base).parts:
                files[pathlib.Path("sdk/crates") / name / path.relative_to(base)] = (
                    path.read_bytes()
                )
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    manifest = re.sub(r"^members = .*$", 'members = ["crates/*"]', manifest, flags=re.MULTILINE)
    manifest = (
        "\n".join(
            line
            for line in manifest.splitlines()
            if not line.startswith(
                ("exclude =", "eden-kernel =", "eden-agent =", "eden-process =", "eden-workspace =")
            )
        )
        + "\n"
    )
    files[pathlib.Path("sdk/Cargo.toml")] = manifest.encode()
    files[pathlib.Path("rust-toolchain.toml")] = (ROOT / "rust-toolchain.toml").read_bytes()
    for name in (*AUTHORS, "invalid"):
        base = ROOT / "tests/contract-authors" / name
        for path in base.rglob("*"):
            if path.is_file() and "target" not in path.relative_to(base).parts:
                data = path.read_bytes()
                if path.name == "Cargo.toml":
                    data = data.replace(
                        b"../../../crates/eden-plugin-sdk", b"../../sdk/crates/eden-plugin-sdk"
                    )
                files[pathlib.Path("authors") / name / path.relative_to(base)] = data
    sync_files(files, destination)


def tree_hashes(root: pathlib.Path) -> dict[str, str]:
    return {
        str(path.relative_to(root)): digest(path)
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def validate_receipt(receipt: dict[str, Any], fingerprint: str) -> None:
    if receipt["source_fingerprint"] != fingerprint:
        raise RuntimeError("Prepared verification source changed; run preparation again")
    for directory, expected in receipt["frozen"].items():
        if tree_hashes(pathlib.Path(directory)) != expected:
            raise RuntimeError(f"Prepared verification artifacts changed: {directory}")


def prepare(output: pathlib.Path | None = None) -> dict[str, Any]:
    global _RECEIPT
    if _RECEIPT is not None:
        return _RECEIPT
    fingerprint = source_fingerprint()
    inherited = os.environ.get("EDEN_VERIFICATION_RECEIPT")
    if inherited:
        receipt = json.loads(pathlib.Path(inherited).read_text(encoding="utf-8"))
        validate_receipt(receipt, fingerprint)
        _RECEIPT = receipt
        return receipt
    output = (output or ROOT / "artifacts/verification").resolve()
    reset_output(output)
    os.environ["EDEN_VERIFICATION_LOG"] = str(output / "prepare")
    started = time.monotonic()
    phases: dict[str, float] = {}
    seed_root = build_target() / "verification/seeds"
    if seed_root.exists():
        shutil.rmtree(seed_root)
    seed_root.mkdir(parents=True)
    # Catch missing native license assets before the expensive Rust build.
    from license_bundle import bundle

    phase = time.monotonic()
    licenses = seed_root / "licenses"
    licenses.mkdir()
    print("Preparing: dependency licenses", flush=True)
    bundle(ROOT, licenses, target())
    phases["licenses"] = time.monotonic() - phase
    phase = time.monotonic()
    print("Preparing: host and all test/example targets", flush=True)
    run(["cargo", "build", "--workspace", "--all-targets", "--locked"], ROOT, timeout=None)
    phases["host_build"] = time.monotonic() - phase
    phase = time.monotonic()
    for controlled in (False, True):
        install(
            seed_root / ("controlled" if controlled else "default"),
            controlled=controlled,
            license_directory=licenses / "third-party-licenses",
        )
    # The probes exercise the installed host but are not part of an installation,
    # so they are built once and kept beside the seeds instead of inside every
    # copy of them.
    examples = build_target() / "verification/examples"
    if examples.exists():
        shutil.rmtree(examples)
    examples.mkdir(parents=True)
    suffix = ".exe" if os.name == "nt" else ""
    for name in EXAMPLES:
        shutil.copy2(
            build_target() / "debug/examples" / (name + suffix), examples / (name + suffix)
        )
    # The search package resolves its worker beside the running executable, so a
    # probe that searches needs that worker in its own directory too.
    shutil.copy2(
        build_target() / "debug" / ("eden-search-worker" + suffix),
        examples / ("eden-search-worker" + suffix),
    )
    frozen = {
        str(seed_root / name): tree_hashes(seed_root / name) for name in ("default", "controlled")
    }
    frozen[str(examples)] = tree_hashes(examples)
    phases["install_and_freeze"] = time.monotonic() - phase
    phase = time.monotonic()
    exported = build_target() / "verification/author-inputs"
    export_authors(exported)
    author_target = build_target() / "verification/author-target"
    compiled = seed_root / "authors"
    print("Preparing: independent SDK authors", flush=True)
    for name, feature in [(name, None) for name in AUTHORS] + [
        ("invalid", feature) for feature in INVALID
    ]:
        args: list[str | pathlib.Path] = [
            "cargo",
            "build",
            "--locked",
            "--target-dir",
            author_target,
        ]
        if feature:
            args.extend(["--features", feature])
        run(args, exported / "authors" / name, timeout=None)
        lib = library("author_" + name.replace("-", "_"))
        folder = compiled / (feature or name)
        folder.mkdir(parents=True)
        shutil.copy2(author_target / "debug" / lib, folder / lib)
    phases["independent_authors"] = time.monotonic() - phase
    command_records = [
        json.loads(path.read_text(encoding="utf-8")) for path in (output / "prepare").glob("*.json")
    ]
    compilation = {
        "cargo_commands": sum(record["command"][0] == "cargo" for record in command_records),
        "compiling_lines": sum(record["cargo_compiling_lines"] for record in command_records),
        "checking_lines": sum(record["cargo_checking_lines"] for record in command_records),
    }
    receipt = {
        "compilation": compilation,
        "source_fingerprint": fingerprint,
        "seeds": str(seed_root),
        "examples": str(examples),
        "exports": str(exported),
        "frozen": frozen,
        "phases": phases,
        "seconds": time.monotonic() - started,
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
    }
    # Prove independent builds did not mutate the frozen host; bind each feature's artifact too.
    validate_receipt(receipt, source_fingerprint())
    receipt["frozen"][str(compiled)] = tree_hashes(compiled)
    receipt["frozen"][str(exported)] = tree_hashes(exported)
    receipt_path = output / "prepared.json"
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    os.environ["EDEN_VERIFICATION_RECEIPT"] = str(receipt_path)
    _RECEIPT = receipt
    print(
        f"Prepared host, installs and independent authors in {receipt['seconds']:.2f}s: {phases}",
        flush=True,
    )
    return receipt


def short_retries(composition: Composition) -> None:
    """Let the acceptance host retry at once instead of sleeping through its backoff.

    The product's default base delay is 2 s and doubles per attempt, which put
    ~16 s of pure sleep in a failing-response scenario. No assertion inspects a
    delay, and the retry count is unchanged, so a green suite still means the
    same control flow; the seed without a coding package is left alone.
    """
    for package in composition["packages"]:
        if package["descriptor"]["package"] != "coding":
            continue
        config = package["config"] if isinstance(package["config"], dict) else {}
        existing = config.get("retry")
        retry: dict[str, Any] = existing if isinstance(existing, dict) else {}
        package["config"] = {**config, "retry": {**retry, "base_delay_ms": 1}}


def example(name: str) -> pathlib.Path:
    """A built probe binary. Probes are not part of a shipped installation."""
    prepared = prepare()
    suffix = ".exe" if os.name == "nt" else ""
    return pathlib.Path(prepared["examples"]) / (name + suffix)


def installed(destination: pathlib.Path, controlled: bool = False) -> pathlib.Path:
    receipt = prepare()
    seed = pathlib.Path(receipt["seeds"]) / ("controlled" if controlled else "default")
    if destination.exists():
        shutil.rmtree(destination)
    shutil.copytree(seed, destination)
    return destination


def author_artifact(name: str, feature: str | None = None) -> pathlib.Path:
    receipt = prepare()
    return (
        pathlib.Path(receipt["seeds"])
        / "authors"
        / (feature or name)
        / library("author_" + name.replace("-", "_"))
    )


def author_sources(scratch: pathlib.Path, names: Sequence[str]) -> None:
    exported = pathlib.Path(prepare()["exports"])
    shutil.copytree(exported / "sdk", scratch / "sdk")
    shutil.copy2(exported / "rust-toolchain.toml", scratch / "rust-toolchain.toml")
    for name in names:
        shutil.copytree(exported / "authors" / name, scratch / "authors" / name)
