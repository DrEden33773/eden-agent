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
| `pnpm doc:check` | pre-push, when the pushed changes affect Rust | none: a broken link and an undocumented public item are both written correctly by hand |

The shared Rust commands cover the root Cargo workspace and every independent project directly under `tests/contract-authors/*` with a Cargo.toml. Clippy checks all targets with the lockfile fixed and warnings denied, and it is also what enforces [`missing_docs`](#doc-comments), because the workspace lint table reaches every member through `lints.workspace = true`. `pnpm doc:check` documents the workspace only — a fixture author under `tests/` that never ships documentation is not what that standard covers — and it denies warnings plus four rustdoc lints: a broken intra-doc link, a bare URL, invalid HTML in doc text, and an unreadable Rust code block. It is cheap enough to belong in the pushed check: a warm run over this workspace takes about 0.2s against Clippy's 2.3s, because `missing_docs` has already made the undocumented item a build error and what remains is only what a build cannot see. `pnpm rust:test:authors` runs `cargo test` for those independent projects, which `cargo test --workspace` excludes; it is a check only and has no hook. Build artifacts share a target directory. Fix commands change the working tree; review the diff and stage the intended changes yourself. Clippy's normal restrictions on dirty/staged trees remain in effect; if appropriate for your reviewed changes, pass `--allow-dirty` or `--allow-staged` explicitly to `pnpm clippy:fix`. Hooks never supply these flags or run fixes.

Markdown uses default markdownlint rules, disables MD013, and limits duplicate-heading checks to siblings. Keep each prose paragraph on one physical line. Generated artifacts, target directories and node_modules are excluded.

## Snapshot behavior

Pre-commit materializes the actual index contents into a temporary directory and runs checks there, using staged manifests and lint configuration. An unstaged fix cannot conceal a staged problem; an unstaged draft cannot invalidate an otherwise valid commit. Intent-to-add entries from `git add -N` are omitted because they are not part of the commit. Deleting Markdown still checks the remaining staged documents. Gitlinks are not traversed. Unresolved entries and tracked non-file entries other than gitlinks are rejected with a diagnostic rather than silently producing an incomplete snapshot.

Pre-push reads the refs and object IDs supplied by Git. It checks affected Python, JavaScript and Markdown, and runs Clippy on each distinct pushed commit that needs a Rust check, including a branch that is not checked out. Existing remote refs are compared to their pushed tips; a new ref, or a remote object unavailable locally, gets a full Rust check. Ref deletions do not compile anything. Uncommitted files and the current index do not affect the checked commit. Hook snapshots do not inherit repository-local Git environment variables into Cargo or build scripts.

Checks write only temporary snapshots and build outputs (`target/hooks` for hooks); they do not format sources, stage files, stash changes or switch the contributor's checkout. Required tools must be available locally. Missing dependencies produce a setup command rather than an automatic installation. Python tools run from the clone-local `.venv`; their installed versions must match the snapshot’s exact development dependencies. Ruff caches and Python bytecode writes are disabled during checks. Changes to the check runner or hook entry points trigger all check families.

## CI and tests

Static Markdown, Ruff, basedpyright standard, Biome and Rust formatting checks run once before native jobs. A PR that changes only the root README or Markdown below `docs/` needs the static gate; every other path, an unknown comparison, and every manual dispatch requires native Linux, Windows and macOS. Changes to the selector and workflow run the complete matrix. The required Quality check rejects failed or missing required jobs, including cancellation. Local hooks remain an early snapshot check.

A main push does not repeat that verification. Pull-request CI runs on GitHub's synthesized merge of the reviewed head into its base, and the squashed commit that lands has that base as its only parent, so the push run compares the landed tree with a fresh merge of the same head: equal trees mean the landed content is the content the review verified. `scripts/ci-proof.mjs` states exactly that comparison and never claims more — tree equivalence is not a re-run of the checks that passed on the merge. The heavy jobs run when the proof cannot be made, when any runner's build cache is missing or cold, and on `workflow_dispatch`; Quality then requires their results, and it accepts the proof as evidence only when the proof succeeded and those jobs were skipped. A missing or unreadable cache state counts as cold, so the cheap path can only be taken when every runner reported a usable cache. The probe covers the pinned toolchain, the cargo registry and the build target, because those decide whether a heavy run downloads and compiles or only revalidates; the pnpm and uv download caches stay with their setup actions, whose key rules the probe would otherwise copy and silently drift from. A cold one is accepted as an extra dependency download in the runs that need it until a heavy main run refreshes it, and the lockfiles make the result identical.

That equivalence depends on branch rules outside this repository: `main` accepts changes only through pull requests (squash-only, with review discussions resolved but no required approving review), evaluates the required status check against the current base (`strict_required_status_checks_policy`), and lists no bypass actors. The strict policy is what rejects a merge whose verified base has moved, so the squashed commit's parent really is the base that pull-request CI merged into, and the empty bypass list is what keeps unreviewed content from landing without the required check. Independent review before merging is a project convention stated in `AGENTS.md`, not a ruleset guarantee. Without those rules the comparison no longer proves that the landed content was verified, and a fork or a changed ruleset must run the heavy jobs on main instead.

Each native job runs verification-infrastructure regressions, dependency-license preflight, a test phase with `--no-fail-fast`, the independent author tests that workspace excludes, Clippy including independent authors, and real Git hook scenarios. On Linux that phase is the unchanged `cargo test --workspace`; on Windows and macOS it is the platform-sensitive subset `eden-process`, `eden-coding-tools`, `eden-search`, `eden-local-history` and `eden-workspace`, because those targets own the branches that differ per operating system: process ownership and shell descendants, link creation, path scope and history locking. The other crates are pure logic or temp-directory fixtures. Linux deliberately keeps the workspace-wide command rather than an equivalent pair of narrower selections: it is the runner that still executes what the other two no longer do, so a crate added tomorrow executes there. Cargo resolves features across the selected packages, so each selection is a different unit graph, and the selection's own cache identity (below) is what writes that graph back: while the identity ignored it, the workspace entry stayed an exact hit and every Windows and macOS run rebuilt the narrowed cascade, which cost 68.2–89.0s and 38.6–48.1s in the three runs of #18 against 4.3s and 5.8s for the same command repeated in a job that already built it. A restored entry still leaves work for the selected members, because the job's own last Cargo invocation is the installed preparation's `cargo build --workspace --all-targets`, which re-unifies features for the workspace graph after the subset has run, and a fresh checkout leaves workspace sources newer than the restored artifacts. Replayed on the Linux development host — subset, installed preparation, reset source timestamps, subset — that phase costs 9.6s for seven crates, where the same command costs 0.1s when nothing runs between it and itself; the warm workspace phase on Linux (21–28s, all workspace members) is the same effect. Clippy still builds every target on every runner, and the acceptance suites below still load and exercise every plugin natively on all three, so this layering narrows which tests execute where, not which code compiles or loads where.

The Windows runner previously compiled zero `eden-process` tests: `crates/eden-process/src/tests.rs` is Unix-only, and the job-object paths it parallels were covered only through an installed acceptance suite. `crates/eden-process/src/windows_tests.rs` now asserts them directly: a completed command's surviving descendants are stopped and the command's own exit code reaches the caller, a requested stop ends the leader as well, descendants started while cleanup enumerates the job are still stopped, and abandoning the tree stops its job. Those tests report a descendant's Windows process id from the shell, because Git Bash's `$!` names an MSYS pid (the native runner measured `$!` = 787 for the process Windows knows as 5284) and because `bin\bash.exe` is a launcher whose child runs the script.

The layering stops executing the other crates' tests off Linux: Windows executes 50 tests where its workspace run executes 119, and macOS 52 where that run executes 121, so 69 tests leave the Windows runner. They are the other crates' tests, and all five `eden-kernel` tests are among them, including the native stop-once test that uses a mock ABI with no platform branch — the coverage loss the layering decision accepted explicitly. Compilation and native loading are unchanged: every plugin is still built by Clippy and loaded and exercised natively by the acceptance suites below on all three runners.

Compilation failures and test failures include the command, phase, exit status, output and timing. Independent phases continue after another phase fails so one platform run can report multiple problems. Cargo continues to later crates after one test target fails; it still returns failure. Node and Python dependency installation have separate steps, so PowerShell cannot replace an earlier failure with a later success. The CI runner writes its running receipt and output while the command is still active.

Run the installed acceptance collection locally with:

```sh
python scripts/verify-all.py --output artifacts/verification
```

Preparation builds the workspace and every example probe once, keeps those probes beside the seeds rather than inside them because an installation does not ship them, assembles isolated installation seeds with their license bundle, and freezes the host before building SDK-only external authors. Four suites run concurrently by default; `--workers 1` or `--workers 2` is available for diagnosis, and the default is a fixed number rather than a CPU count so local and CI timings stay comparable. Each invalid ABI variant has its own copied artifact. Stable exported inputs preserve unchanged file timestamps, remove deleted inputs and contain no kernel, CLI or first-party plugin source. Cargo checks the current source and locks on every preparation. Individual `verify*.py` entry points remain supported and use the same preparation.

Five suites execute in separate processes with separate installation copies, project directories, state and loopback ports. Controlled HTTP fixtures stop through a wakeup socket, join every request handler, and inspect errors after those handlers finish; late handler failures cannot produce a green close. Every assertion executes again, including explicit source-package compilation, reopen behavior and unknown namespaced interop. Source fingerprints and artifact digests reject preparation reuse after changed sources or native bytes; prior verification JSON is removed before execution. The final distribution is copied from the frozen default installation only after every suite passes. Reports live in `artifacts/verification/timings.json`, with per-suite results and per-command stdout/stderr; the original `artifacts/*verification.json` paths remain available.

Each native runner also extracts its actual release archive into a fresh directory beneath an unrelated receipt file. `scripts/verify-archive.py` compares every file and its bytes against the frozen distribution, checks executable permission bits on POSIX, and executes the packaged CLI's `--version` and `resources list` commands with an isolated project and global directory. Windows execution validates native executable access. These commands make no Provider calls. `artifacts/ci/archive-verification.json` binds the archive, host and plugin hashes to the source commit and native target; it is a compact downloadable proof alongside the raw native diagnostics.

CI caches the pinned Rust toolchain, pnpm store, uv downloads, Cargo registry/Git sources and native build outputs separately. A build-cache entry names one compatible build context — OS, architecture, runner image, toolchain, manifests, locks, CI profile identity and the test selection that runner executes — and later source revisions restore it, because the commit is recorded in `artifacts/ci/cache-inputs.json` instead of the key; a key that carried the commit created one entry per run and could never hit one. The test selection belongs to that context because Cargo resolves features across the packages a command selects: the narrowed subset is a different unit graph from the workspace one, and while the identity ignored it, the workspace entry stayed an exact hit, so the narrowed graph was never written back and every Windows and macOS run rebuilt it (`cargo-target-v3` is deliberately not restored under `cargo-target-v4`, and no fallback is declared: the first heavy main run on the new key is cold, which is the v4 baseline — that rekeys every runner, Linux included, although its command did not change; `cargo-target-v5` is the same deliberate rekey for the `eden-fmt` workspace member, which joins every runner's unit graph while the test selection stays unchanged). Each runner's selection is declared once in `scripts/ci-cache.py` against the runner label that both the heavy job and the cache-state probe pass, so the probe looks for the entry that job writes; `scripts/test_ci_cache.py` fails when the declaration and the workflow's command, runner conditions or probe inputs drift apart. An empty package list is the unchanged `cargo test --workspace`. Only a main push or a manual dispatch writes caches: a pull-request run's entries live under `refs/pull/<n>/merge`, which no other pull request and no branch can restore, so writing them only evicts the entries the main runs restore. Because a job-level `cache-mode` has to be a literal value and not an expression, the pull-request and main paths call the same native verification workflow from two jobs that declare `read` and `write` respectively instead of sharing one job. Cargo validates restored outputs; CI does not restore source timestamps or cache test verdicts. CI disables incremental compilation and uses debug level 1 to reduce cache transfer size while retaining line information and debug assertions. Cache identities, compilation counts and phase durations are uploaded under `artifacts/ci` on success or failure. A cache hit alone is not evidence of a passed test or an avoided rebuild.

Measure cold and warm runs on the same source and toolchain. Compare preparation, compilation, execution and cache transfer separately; preserve the failed baseline and the original regression assertions. Do not speed up a failing test by adding retries, broad sleeps, skips or weaker assertions. The acceptance host is configured with a 1 ms retry base delay instead of the product's 2 s default: the suites assert which retries happen and never how long they took. `python -m unittest discover -s scripts -p 'test_*.py' -v` checks license packaging, preparation invalidation, installation isolation and failure diagnostics without compiling the workspace.

## Python and JavaScript policy

`pyproject.toml` and `uv.lock` pin Ruff 0.14.3 and basedpyright 1.40.1. Ruff formats at 100 columns and checks E4/E7/E9/F/I/UP/B. Basedpyright uses `standard`, Python 3.12 and `All` platforms, with warnings failing the command. Type annotations describe subprocess text results, filesystem paths, HTTP callbacks, dynamic module interfaces and JSON boundaries. There is no blanket diagnostic suppression or baseline file. Commands may also be run separately with `python:format:check`, `python:lint` and `python:types`.

Biome 2.5.3 is pinned in pnpm and applies its recommended lint preset, formatting and import organization to mjs. Build outputs, dependencies and local virtual environments are excluded from checks. `.vscode/settings.json` selects Ruff, Biome and rust-analyzer for format-on-save; install the recommended extensions to use those editor integrations.

## Macro call formatting

`rustfmt` never enters a `json!` body that uses object syntax, because `"key": value` is not a Rust expression, and it never enters a brace-delimited statement macro such as `tokio::select!` at all. `eden-fmt` is a workspace member that formats exactly those bodies: it lowers a body into the Rust expression rustfmt does parse, formats the file, and lifts the result back into the DSL. Every layout decision is still rustfmt's, so the tool is a compatibility layer rather than a second pretty-printer. It refuses to touch a file whose target macro it cannot read (`pnpm rust:check` then reports the position and leaves the file alone), and its output is a fixed point of both `cargo fmt` and itself. It also refuses a file that still carries a long string as a backslash continuation, a decision no formatter can make for the author; [String literals](#string-literals) states the shapes it accepts. Build it once with `cargo build --locked -p eden-fmt`; `pnpm rust:check` and `pnpm rust:fmt` run it for the root workspace and for every independent author.

The shape it enforces is the shape rustfmt gives a struct literal and an array:

- A `json!` object stays on one line while it fits; otherwise it is written one member per line, indented four spaces, with a trailing comma after the last member.
- A `json!` array follows the same rule: one line while it fits, otherwise one element per line.
- A value is formatted by rustfmt, so a long expression breaks the way rustfmt breaks it.
- A `tokio::select!` body keeps the tokio grammar: `biased;` first when present, then `pattern = future => handler,` arms, an arm with a guard written `pattern = future, if condition => handler,`, and `else => handler,` last. A block handler keeps its block, and the separator comma is written even where tokio allows it to be omitted.

Write it this way:

```rust
let short = json!({ "type": "object", "required": ["command"] });
let long = json!({
    "type": "object",
    "properties": { "command": { "type": "string" } },
    "additionalProperties": false,
});
tokio::select! {
    biased;
    _ = cancel.cancelled() => Err(Fault::new("Cancelled", "composition")),
    reply = request(&settings.endpoint) => Ok(reply),
}
```

Not this way:

```rust
let short = json!({"type":"object","required":["command"]});
let long = json!({
"type":"object",
"properties":{"command":{"type":"string"}},
});
```

`pnpm rust:fmt` applies both formatters: `cargo fmt` for the Rust around a macro, and `eden-fmt write` for the macro bodies.

## String literals

A formatter rewrites layout, never a literal, so how a long string is carried is a decision the author has to make and no formatter can enforce by rewriting. `eden-fmt check` therefore reads every literal in the file and refuses a backslash line continuation, in `write` mode too, without touching the file. It reports the exact position and the literal's opening.

Put the text in one of these shapes instead:

- `concat!(...)` when the value must stay exactly what it is. Each member is a literal; rustfmt gives every member its own line, and the value is their concatenation.
- A byte string cannot be a `concat!` argument, so carry it as `concat!("...", "...").as_bytes()`.
- A raw string laid out by meaning when the text is read by a person and the line breaks belong to it. This changes the value, so it is a change to the message, not to its formatting.

A format string keeps its `{name}` placeholder, but a `concat!` is expanded before `format_args!` sees it, so a capture that used to be implicit has to become an explicit argument:

```rust
// not this: `format_args!` cannot capture through `concat!`
format!(concat!("round {n} of ", "the retry budget"))
// this
format!(concat!("round {n} of ", "the retry budget"), n = n)
```

Width is a target for layout decisions, not a per-line limit, and it is not enforced at all inside a macro body: a long string literal there can pass the target once the body is indented to the standard above, because neither formatter splits a string literal. Re-run `pnpm rust:fmt` and inspect the affected block, then run `pnpm rust:check`. Changing the configured width or switching to nightly is not required.

Outside a macro body the same literal costs more than tidiness. rustfmt cannot split a string, so when it has to lay out a call or expression and no shape fits, it stops formatting that statement and writes the original text through unchanged — including everything else inside it. `pnpm rust:check` stays green, and a later edit anywhere in that statement is invisible to it. This repository has been in exactly that state twice (`crates/eden-agent/src/lib.rs` and `plugins/coding/src/lib.rs`), so a literal whose line would exceed the width is carried with `concat!(...)` even where the target is only advisory. rustfmt reports it directly with `error_on_line_overflow = true`, which is nightly-only; on the stable toolchain the check is an author's: mis-indent one line of a region that looks hand-written and confirm `cargo fmt --check` fails.

## Doc comments

A doc comment carries what the signature cannot. A parameter name, a return type and a struct field already state *what* something is; a comment that repeats them is negative value, because it is one more thing to keep true that no compiler checks. What has no other home is the decision behind the code — why a fallback exists, which invariant a caller can rely on, what a deliberate omission protects — and that is what the comment is for. `#[workspace.lints.rust] missing_docs = "deny"` makes the coverage requirement a build error; nothing can enforce that a comment is worth reading, so this section states the bar instead.

Every public item of the workspace crates carries a comment. A module carries a `//!` header stating the part it plays, and its public items each state their intent or their contract. An item that is not public is documented when its reason is not obvious from its own body. Where a subject has a model rather than a single item — an ownership rule, an exit-status rule, a lifecycle — the crate header states it once and the item comments stay short and local: [`eden-process`](../crates/eden-process/src/lib.rs) defines the exit-status model in its module header, and [`eden-cli`](../crates/eden-cli/src/cli.rs) shows the per-item register, where a comment explains why a payload without a level still prints and why a fallback reads a literal token.

A caller who can be broken by an invariant is owed that invariant in the documentation of the item they call. No doctest is used here: no example is compiled or run. Test modules, test helper files and `tests/contract-authors/*` are outside this standard — their documentation is their own README, and their comments are not built for readers of the library.

A field or a variant is different from an item, because its own name and type already state what it is. `pub parent_id: Option<u64>` is complete without prose, and `MissingDependency { contract: String }` says what it carries. Such a group takes `#[allow(missing_docs)]` at the item with a word on why, which is a statement about that item and not a silence over the module. Prose is owed when the name and type are not the whole meaning: what a `None` means beyond absence, whether a zero is unknown or really zero, where an unqualified value is resolved, or that a variant exists for a reason the caller has to know — [`eden-process`](../crates/eden-process/src/lib.rs)'s `Stop` documents exactly the absent code its fields cannot distinguish.

The one escape is honesty. When no real intent can be stated for an item, it is not made public, or it is marked `#[doc(hidden)]` and left as internal implementation; the gate accepts `#[doc(hidden)]` in place of a comment. Writing a sentence that restates the name or the type to satisfy the lint is the failure this section exists to prevent, and it hides the more useful finding: an item nobody can explain is usually an item that should not have been exported. Do not ask a check to measure comment length or coverage percentage, because any such number is one an author can raise by padding.

## Rustfmt limitations

`rustfmt.toml` sets edition and style edition 2024. It does not set a width: 100 columns is rustfmt's own default, and `eden-fmt` passes the same value explicitly because it also has to format the independent author projects, which have no `rustfmt.toml` of their own. All commands continue using the pinned stable Rust toolchain. A passing `cargo fmt --check` is not a strict line-length guarantee: rustfmt may leave an enclosing closure or block untouched when an unbreakable macro string overflows, as tracked in [rustfmt #6870](https://github.com/rust-lang/rustfmt/issues/6870). Editor format-on-save can encounter the same limitation.

Four more limits are worth knowing before reading `eden-fmt`'s output as a verdict on style. A comment written where a lowered arm has no room for it — between a pattern and its `=`, between `=` and the future, between the future and `=>`, or between `=>` and the handler — is refused rather than moved, and so is a multi-argument closure used as a future or a handler. A piece rustfmt moved to its own line keeps the indentation the lowered form gave it, which can sit one level deeper than the lifted arm suggests. A refusal raised while lifting names a position in the intermediate lowercase text rather than in the author's file, although the file is still left untouched. Any macro named `select!` is treated as tokio's grammar, and rustfmt itself drops a redundant leading `|` from an or-pattern.
