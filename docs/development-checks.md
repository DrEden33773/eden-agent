# Development checks

The public repository owns its development tools and hooks. It does not use dependencies or configuration from a parent workspace.

## Setup

Install Node.js 22+, pnpm 10.32.1, Python 3.12+ and uv 0.9.7, then run these commands in each clone:

```sh
pnpm install --frozen-lockfile
uv sync --locked
pnpm hooks:install
```

The explicit installer sets this repository's local `core.hooksPath` to `.githooks`. It does not change global Git configuration or run automatically during dependency installation. Git for Windows runs the same shell entry points; `.gitattributes` keeps their line endings LF. The Rust toolchain remains pinned by `rust-toolchain.toml`.

## Checks and fixes

| Check | Hook | Explicit repair |
| --- | --- | --- |
| `pnpm markdown:check` | pre-commit and pre-push, when Markdown or its root configuration changes | `pnpm markdown:fix` |
| `pnpm rust:check` | pre-commit, when Rust sources, manifests or Rust configuration change | `pnpm rust:fmt` |
| `pnpm clippy:check` | pre-push, when the pushed changes affect Rust | `pnpm clippy:fix` |
| `pnpm python:check` | pre-commit and pre-push, when Python/configuration changes | `pnpm python:format`; fix lint/types explicitly |
| `pnpm js:check` | pre-commit and pre-push, when mjs/configuration changes | `pnpm js:fix` |

The shared Rust commands cover the root Cargo workspace and every independent project directly under `tests/contract-authors/*` with a Cargo.toml. Clippy checks all targets with the lockfile fixed and warnings denied. Build artifacts share a target directory. Fix commands change the working tree; review the diff and stage the intended changes yourself. Clippy's normal restrictions on dirty/staged trees remain in effect; if appropriate for your reviewed changes, pass `--allow-dirty` or `--allow-staged` explicitly to `pnpm clippy:fix`. Hooks never supply these flags or run fixes.

Markdown uses default markdownlint rules, disables MD013, and limits duplicate-heading checks to siblings. Keep each prose paragraph on one physical line. Generated artifacts, target directories and node_modules are excluded.

## Snapshot behavior

Pre-commit materializes the actual index contents into a temporary directory and runs checks there, using staged manifests and lint configuration. An unstaged fix cannot conceal a staged problem; an unstaged draft cannot invalidate an otherwise valid commit. Intent-to-add entries from `git add -N` are omitted because they are not part of the commit. Deleting Markdown still checks the remaining staged documents. Gitlinks are not traversed. Unresolved entries and tracked non-file entries other than gitlinks are rejected with a diagnostic rather than silently producing an incomplete snapshot.

Pre-push reads the refs and object IDs supplied by Git. It checks affected Python, JavaScript and Markdown, and runs Clippy on each distinct pushed commit that needs a Rust check, including a branch that is not checked out. Existing remote refs are compared to their pushed tips; a new ref, or a remote object unavailable locally, gets a full Rust check. Ref deletions do not compile anything. Uncommitted files and the current index do not affect the checked commit. Hook snapshots do not inherit repository-local Git environment variables into Cargo or build scripts.

Checks write only temporary snapshots and build outputs (`target/hooks` for hooks); they do not format sources, stage files, stash changes or switch the contributor's checkout. Required tools must be available locally. Missing dependencies produce a setup command rather than an automatic installation. Python tools run from the clone-local `.venv`; their installed versions must match the snapshot’s exact development dependencies. Ruff caches and Python bytecode writes are disabled during checks. Changes to the check runner or hook entry points trigger all check families.

## CI and tests

Static Markdown, Ruff, basedpyright standard, Biome and Rust formatting checks run once before native jobs. A PR that changes only the root README or Markdown below `docs/` needs the static gate; every other path, an unknown comparison, and every main push requires native Linux, Windows and macOS. Changes to the selector and workflow run the complete matrix. The required Quality check rejects failed or missing required jobs, including cancellation. Local hooks remain an early snapshot check.

Each native job runs verification-infrastructure regressions, dependency-license preflight, all workspace core/process tests with `--no-fail-fast`, Clippy including independent authors, and real Git hook scenarios. These checks retain native coverage of Windows locks, process trees, paths, line endings and dynamic libraries. Compilation failures and test failures include the command, phase, exit status, output and timing. Independent phases continue after another phase fails so one platform run can report multiple problems. Cargo continues to later crates after one test target fails; it still returns failure. Node and Python dependency installation have separate steps, so PowerShell cannot replace an earlier failure with a later success. The CI runner writes its running receipt and output while the command is still active.

Run the installed acceptance collection locally with:

```sh
python scripts/verify-all.py --workers 2 --output artifacts/verification
```

Preparation builds the workspace and examples once, assembles isolated installation seeds with their license bundle, and freezes the host before building SDK-only external authors. Each invalid ABI variant has its own copied artifact. Stable exported inputs preserve unchanged file timestamps, remove deleted inputs and contain no kernel, CLI or first-party plugin source. Cargo checks the current source and locks on every preparation. Individual `verify*.py` entry points remain supported and use the same preparation.

Five suites execute in separate processes with separate installation copies, project directories, state and loopback ports. Controlled HTTP fixtures stop through a wakeup socket, join every request handler, and inspect errors after those handlers finish; late handler failures cannot produce a green close. Every assertion executes again, including explicit source-package compilation, reopen behavior and unknown namespaced interop. Source fingerprints and artifact digests reject preparation reuse after changed sources or native bytes; prior verification JSON is removed before execution. The final distribution is copied from the frozen default installation only after every suite passes. Reports live in `artifacts/verification/timings.json`, with per-suite results and per-command stdout/stderr; the original `artifacts/*verification.json` paths remain available.

CI caches the pinned Rust toolchain, pnpm store, uv downloads, Cargo registry/Git sources and native build outputs separately. Build caches include OS, architecture, toolchain, manifests, locks and CI profile identity, and can restore compatible dependencies across source revisions. Cargo validates restored outputs; CI does not restore source timestamps or cache test verdicts. CI disables incremental compilation and uses debug level 1 to reduce cache transfer size while retaining line information and debug assertions. Cache identities, compilation counts and phase durations are uploaded under `artifacts/ci` on success or failure. A cache hit alone is not evidence of a passed test or an avoided rebuild.

Measure cold and warm runs on the same source and toolchain. Compare preparation, compilation, execution and cache transfer separately; preserve the failed baseline and the original regression assertions. Do not speed up a failing test by adding retries, broad sleeps, skips or weaker assertions. `python -m unittest discover -s scripts -p 'test_*.py' -v` checks license packaging, preparation invalidation, installation isolation and failure diagnostics without compiling the workspace.

## Python and JavaScript policy

`pyproject.toml` and `uv.lock` pin Ruff 0.14.3 and basedpyright 1.40.1. Ruff formats at 100 columns and checks E4/E7/E9/F/I/UP/B. Basedpyright uses `standard`, Python 3.12 and `All` platforms, with warnings failing the command. Type annotations describe subprocess text results, filesystem paths, HTTP callbacks, dynamic module interfaces and JSON boundaries. There is no blanket diagnostic suppression or baseline file. Commands may also be run separately with `python:format:check`, `python:lint` and `python:types`.

Biome 2.5.3 is pinned in pnpm and applies its recommended lint preset, formatting and import organization to mjs. Build outputs, dependencies and local virtual environments are excluded from checks. `.vscode/settings.json` selects Ruff, Biome and rust-analyzer for format-on-save; install the recommended extensions to use those editor integrations.

## Rustfmt limitations

`rustfmt.toml` explicitly sets edition/style edition 2024 and a 100-column target. All commands continue using the pinned stable Rust toolchain. A passing `cargo fmt --check` is not a strict line-length guarantee: rustfmt may leave an enclosing closure or block untouched when an unbreakable macro string overflows, as tracked in [rustfmt #6870](https://github.com/rust-lang/rustfmt/issues/6870). Editor format-on-save can encounter the same limitation.

Split long strings with Rust backslash-newline continuations where their contents must stay identical, and lay out complex macro arguments over multiple lines. Re-run `pnpm rust:fmt` and inspect the affected block, then run `pnpm rust:check`. Long literals in fixtures and protocol messages need the same attention as ordinary code; changing formatter width or switching to nightly is not required.
