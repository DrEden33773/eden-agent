# Development checks

The public repository owns its development tools and hooks. It does not use dependencies or configuration from a parent workspace.

## Setup

Install Node.js 22+ and pnpm 10.32.1, then run these commands in each clone:

```sh
pnpm install --frozen-lockfile
pnpm hooks:install
```

The explicit installer sets this repository's local `core.hooksPath` to `.githooks`. It does not change global Git configuration or run automatically during dependency installation. Git for Windows runs the same shell entry points; `.gitattributes` keeps their line endings LF. The Rust toolchain remains pinned by `rust-toolchain.toml`.

## Checks and fixes

| Check | Hook | Explicit repair |
| --- | --- | --- |
| `pnpm markdown:check` | pre-commit, when Markdown or its root configuration changes | `pnpm markdown:fix` |
| `pnpm rust:check` | pre-commit, when Rust sources, manifests or Rust configuration change | `pnpm rust:fmt` |
| `pnpm clippy:check` | pre-push, when the pushed changes affect Rust | `pnpm clippy:fix` |

The shared Rust commands cover the root Cargo workspace and every independent project directly under `tests/contract-authors/*` with a Cargo.toml. Clippy checks all targets with the lockfile fixed and warnings denied. Build artifacts share a target directory. Fix commands change the working tree; review the diff and stage the intended changes yourself. Clippy's normal restrictions on dirty/staged trees remain in effect; if appropriate for your reviewed changes, pass `--allow-dirty` or `--allow-staged` explicitly to `pnpm clippy:fix`. Hooks never supply these flags or run fixes.

Markdown uses default markdownlint rules, disables MD013, and limits duplicate-heading checks to siblings. Keep each prose paragraph on one physical line. Generated artifacts, target directories and node_modules are excluded.

## Snapshot behavior

Pre-commit materializes the actual index contents into a temporary directory and runs checks there, using staged manifests and lint configuration. An unstaged fix cannot conceal a staged problem; an unstaged draft cannot invalidate an otherwise valid commit. Intent-to-add entries from `git add -N` are omitted because they are not part of the commit. Deleting Markdown still checks the remaining staged documents. Unresolved entries and tracked non-file entries are rejected with a diagnostic rather than silently producing an incomplete snapshot.

Pre-push reads the refs and object IDs supplied by Git. It runs Clippy on each distinct pushed commit that needs a Rust check, including a branch that is not checked out. Existing remote refs are compared to their pushed tips; a new ref, or a remote object unavailable locally, gets a full Rust check. Ref deletions do not compile anything. Uncommitted files and the current index do not affect the checked commit. Hook snapshots do not inherit repository-local Git environment variables into Cargo or build scripts.

Checks write only temporary snapshots and build outputs (`target/hooks` for hooks); they do not format sources, stage files, stash changes or switch the contributor's checkout. Required tools must be available locally. Missing Markdown dependencies produce a setup command rather than an automatic installation.

## CI and tests

The native matrix runs the same Markdown, formatting and Clippy commands on Linux, Windows and macOS, plus `pnpm hooks:test` for real Git commit/push regression scenarios. Local hooks provide early feedback; the required Quality check remains the merge gate even when a local hook was not installed or was bypassed.
