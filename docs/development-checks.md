# Development checks

The public repository owns its development tools and hooks. It does not use dependencies or configuration from a parent workspace.

## Setup

Install Node.js 22+, pnpm 10.32.1, Python 3.12+ and uv 0.9.7, then run these commands in each clone:

```sh
pnpm install --frozen-lockfile
uv sync --locked
pnpm hooks:install
node scripts/install-formatter.mjs
cargo build --locked -p eden-fmt
```

The explicit installer sets this repository's local `core.hooksPath` to `.githooks`. It does not change global Git configuration or run automatically during dependency installation. Git for Windows runs the same shell entry points; `.gitattributes` keeps their line endings LF. The build, test and Clippy compiler remains Rust 1.98.1 from `rust-toolchain.toml`. The formatter installer reads `rustfmt-toolchain` and installs official `nightly-2026-09-21` with rustfmt. No patched formatter is used. Rebuild eden-fmt after pulling formatter changes; it embeds the pin, and snapshot checks reject a binary with a different pin. VS Code/rust-analyzer uses the checked-in `scripts/format-editor.mjs` stdin entry; open the product root as the editor workspace, including when editing independent author projects.

## Checks and fixes

| Check | Hook | Explicit repair |
| --- | --- | --- |
| `pnpm markdown:check` | pre-commit and pre-push, when Markdown or its root configuration changes | `pnpm markdown:fix` |
| `pnpm rust:check` | pre-commit and pre-push, when Rust sources, manifests or Rust configuration change | `pnpm rust:fmt` |
| `pnpm clippy:check` | pre-push, when the pushed changes affect Rust | `pnpm clippy:fix` |
| `pnpm python:check` | pre-commit and pre-push, when Python/configuration changes | `pnpm python:format`; fix lint/types explicitly |
| `pnpm biome:check` | pre-commit and pre-push, when JS/JSX/MJS/TS/TSX/CSS/HTML or related configuration changes | `pnpm biome:fix` |
| `pnpm web:types` | pre-push, when Web sources, tsconfig or dependency inputs change | fix type errors explicitly |
| `pnpm web:build` | CI and explicit local checks | fix build errors explicitly |
| `pnpm doc:check` | pre-push, when the pushed changes affect Rust | none: a broken link and an undocumented public item are both written correctly by hand |

`pnpm js:check` is the compatibility aggregate for Biome and `web:check`; `pnpm js:fix` aliases `biome:fix`. The Web package's own `build` runs Vite only; use root `web:check` for type checking plus build.

The shared Rust commands cover the root Cargo workspace and every independent project directly under `tests/contract-authors/*` with a Cargo.toml. Clippy checks all targets with the lockfile fixed and warnings denied, and it is also what enforces [`missing_docs`](#doc-comments), because the workspace lint table reaches every member through `lints.workspace = true`. `pnpm doc:check` documents the workspace only — a fixture author under `tests/` that never ships documentation is not what that standard covers — and it denies warnings plus four rustdoc lints: a broken intra-doc link, a bare URL, invalid HTML in doc text, and an unreadable Rust code block. It is cheap enough to belong in the pushed check: a warm run over this workspace takes about 0.2s against Clippy's 2.3s, because `missing_docs` has already made the undocumented item a build error and what remains is only what a build cannot see. `pnpm rust:test:authors` runs enabled Cargo test targets for those independent projects, which `cargo test --workspace` excludes; it is a check only and has no hook. Build artifacts share a target directory. Fix commands change the working tree; review the diff and stage the intended changes yourself. Clippy's normal restrictions on dirty/staged trees remain in effect; if appropriate for your reviewed changes, pass `--allow-dirty` or `--allow-staged` explicitly to `pnpm clippy:fix`. Hooks never supply these flags or run fixes.

Markdown uses default markdownlint rules, disables MD013, and limits duplicate-heading checks to siblings. Keep each prose paragraph on one physical line. Generated artifacts, target directories and node_modules are excluded.

## Snapshot behavior

Pre-commit materializes the actual index contents into a temporary directory and runs checks there, using staged manifests and lint configuration. An unstaged fix cannot conceal a staged problem; an unstaged draft cannot invalidate an otherwise valid commit. Intent-to-add entries from `git add -N` are omitted because they are not part of the commit. Deleting Markdown still checks the remaining staged documents. Gitlinks are not traversed. Unresolved entries and tracked non-file entries other than gitlinks are rejected with a diagnostic rather than silently producing an incomplete snapshot.

Pre-push reads the refs and object IDs supplied by Git. It checks affected Python, JavaScript and Markdown, and runs Clippy on each distinct pushed commit that needs a Rust check, including a branch that is not checked out. Existing remote refs are compared to their pushed tips; a new ref, or a remote object unavailable locally, gets a full Rust check. Ref deletions do not compile anything. Uncommitted files and the current index do not affect the checked commit. Hook snapshots do not inherit repository-local Git environment variables into Cargo or build scripts.

Checks write only temporary snapshots and build outputs (`target/hooks` for hooks); they do not format sources, stage files, stash changes or switch the contributor's checkout. Required tools must be available locally. Missing dependencies produce a setup command rather than an automatic installation. Python tools run from the clone-local `.venv`; their installed versions must match the snapshot’s exact development dependencies. Ruff caches and Python bytecode writes are disabled during checks. Changes to the check runner or hook entry points trigger all check families.

## CI and tests

A lightweight scope job reads the complete Git diff before tool installation: PR base/head uses a three-dot comparison, while a main push uses before/after. Deletions, both rename paths and mode changes participate. An unavailable comparison fails scope; an empty comparison, unknown input or manual dispatch selects all product check families. README, AGENTS and `docs/*.md` select Markdown. Python and non-Web frontend sources select their static checks and native acceptance; Web sources select Biome, types and build without native acceptance; Rust sources select formatting/rustdoc and native acceptance. Mixed changes take the union.

Git hook behavior has a separate `hooks` selection. Its existing three test files run with a concurrency limit of three, retaining all 38 cases and the order within each file. Ordinary product source changes do not run hook regressions. Hooks, shared check/CI entry points, workflows, formatter sources, Cargo manifests/locks, toolchain and lint configuration, Node/Python dependency inputs and unknown paths select them. Each native job requires the hook step to succeed when selected and to be skipped otherwise; missing, cancelled, failed or unexpected execution fails its gate. Static checks still check the whole selected language. Quality requires successful scope and the complete selected check matrix. Static checks and each selected native platform run concurrently in one fail-fast matrix; a failed member cancels its siblings. Quality runs outside that matrix and rejects failure, cancellation, missing results and unexpected skips. Documentation-only changes select just the static row.

A main push can reuse the PR's selected coverage when `scripts/ci-proof.mjs` proves that the landed tree equals the merge of the reviewed head into the landed parent. This is inherited evidence, not a fresh run or wider coverage. Cache warmth never changes the verdict: a successful proof skips static/native acceptance even with cold caches; without a proof the push runs checks selected from its before/after inputs. Manual dispatch selects every product family.

After a successful main proof, a separate cache-maintenance matrix looks up the current target key without downloading an exact hit. On a miss it restores compatible objects and runs `scripts/ci-warm.py`: Clippy, the platform's Cargo test graph with `--no-run`, enabled author test graphs with `--no-run`, and installed preparation. It executes no test bodies or acceptance suites and publishes no distribution archive. Toolchain and registry caches are also replenished. Cache-only jobs run `pnpm fetch --frozen-lockfile` to populate a newly keyed pnpm store without executing product checks; setup-node can then save a real store after a lockfile update. This job is outside Quality's dependency list; its build receipts describe cache maintenance, not product test success. A maintenance failure remains visible as its own failed job. Only trusted `main` runs save caches; PR save steps are skipped before compression. No scheduled keepalive or PR cache writer is used.

That equivalence depends on branch rules outside this repository: `main` accepts changes only through pull requests (squash-only, with review discussions resolved but no required approving review), evaluates the required status check against the current base (`strict_required_status_checks_policy`), and lists no bypass actors. The strict policy is what rejects a merge whose verified base has moved, so the squashed commit's parent really is the base that pull-request CI merged into, and the empty bypass list is what keeps unreviewed content from landing without the required check. Independent review before merging is a project convention stated in `AGENTS.md`, not a ruleset guarantee. Without those rules the comparison no longer proves that the landed content was verified, and a fork or a changed ruleset must run the heavy jobs on main instead.

Each native verification job runs infrastructure regressions, license preflight, Clippy including independent authors, the selected hook regressions, native Rust tests and installed acceptance. Linux executes `cargo test --workspace --locked --no-fail-fast`. Windows/macOS execute `eden-process`, `eden-coding-tools`, `eden-search`, `eden-local-history`, `eden-workspace`, `eden-model-access` and `eden-fmt`. They retain native process ownership, filesystem/credential behavior and pinned formatter coverage. Clippy still checks all targets on all three systems; every installed plugin and external author is built and exercised natively.

`pnpm rust:test:authors` discovers author workspaces through their manifests and uses Cargo metadata to select enabled test/doctest targets. Empty fixture targets declare `test = false` and `doctest = false`; the embedded executable disables its empty unit harness. The seven `coding-replacements` tests still execute on all platforms. A new integration test target is enabled by Cargo's default discovery; when adding unit tests to an opted-out target, re-enable `test` in that manifest. These flags do not remove independent author compilation or installed loading.

Compilation failures and test failures include the command, phase, exit status, output and timing. After a failed step, later heavy phases do not start; short result recording and diagnostic uploads still run. Cancellation includes runner signal handling and cleanup, so other members may finish an already running command before stopping. Partial receipts and uploaded archives do not establish Quality success. Cargo continues to later crates after one test target fails; it still returns failure. Node and Python dependency installation have separate steps, so PowerShell cannot replace an earlier failure with a later success. The CI runner writes its running receipt and output while the command is still active.

Run the installed acceptance collection locally with:

```sh
python scripts/verify-all.py --output artifacts/verification
```

Preparation builds workspace libraries, binaries, examples and the named RPC test target in one Cargo selection, without compiling every unrelated test/benchmark. It resolves the RPC executable from that invocation’s Cargo JSON artifact messages and copies it into the frozen probe directory; installed RPC checks execute that exact harness with their controlled composition instead of invoking Cargo again. Preparation keeps those probes beside the seeds rather than inside them because an installation does not ship them, assembles isolated installation seeds with their license bundle, and freezes the host before building SDK-only external authors. Four suites run concurrently by default; `--workers 1` or `--workers 2` is available for diagnosis, and the default is a fixed number rather than a CPU count so local and CI timings stay comparable. Each invalid ABI variant has its own copied artifact. Stable exported inputs preserve unchanged file timestamps and remove deleted inputs. External authors use an exported SDK source tree without CLI or first-party plugin implementation sources. Cargo checks the current source and locks on every preparation. Individual `verify*.py` entry points remain supported and use the same preparation.

Ten suites execute in separate processes with separate installation copies, project directories, state and loopback ports. Controlled HTTP fixtures stop through a wakeup socket, join every request handler, and inspect errors after those handlers finish; late handler failures cannot produce a green close. Every assertion executes again, including explicit source-package compilation, reopen behavior and unknown namespaced interop. Source fingerprints and artifact digests reject preparation reuse after changed sources or native bytes; prior verification JSON is removed before execution. The final distribution is copied from the frozen default installation only after every suite passes. Reports live in `artifacts/verification/timings.json`, with per-suite results and per-command stdout/stderr; the original `artifacts/*verification.json` paths remain available.

Each native runner also extracts its actual release archive into a fresh directory beneath an unrelated receipt file. `scripts/verify-archive.py` compares every file and its bytes against the frozen distribution, checks executable permission bits on POSIX, and executes the packaged CLI's `--version` and `resources list` commands with an isolated project and global directory. Windows execution validates native executable access. These commands make no Provider calls. `artifacts/ci/archive-verification.json` binds the archive, host and plugin hashes to the source commit and native target; it is a compact downloadable proof alongside the raw native diagnostics.

CI caches the pinned Rust toolchain, pnpm store, uv downloads, Cargo registry/Git sources and native build outputs separately. Target compatibility includes OS, architecture, runner image, toolchain/profile environment and platform test selection; the exact key also includes manifests and lockfiles. `scripts/ci-cache.py` owns this identity, and regression tests check that workflow test selections and target paths agree. Source commit identity is recorded in receipts rather than creating an unbounded cache entry per commit. Existing entries are immutable; a new graph/input key is populated on main and later PRs can restore it. Cargo validates restored outputs: source timestamps are not restored and test verdicts are not cached. CI disables incremental compilation and uses debug level 1. A cache hit alone proves neither passed tests nor avoided recompilation.

Measure cold and warm runs on the same source and toolchain. Compare preparation, compilation, execution and cache transfer separately; preserve the failed baseline and the original regression assertions. Do not speed up a failing test by adding retries, broad sleeps, skips or weaker assertions. The acceptance host is configured with a 1 ms retry base delay instead of the product's 2 s default: the suites assert which retries happen and never how long they took. `python -m unittest discover -s scripts -p 'test_*.py' -v` checks license packaging, preparation invalidation, installation isolation and failure diagnostics without compiling the workspace.

## Python and JavaScript policy

`pyproject.toml` and `uv.lock` pin Ruff 0.14.3 and basedpyright 1.40.1. Ruff formats at 100 columns and checks E4/E7/E9/F/I/UP/B. Basedpyright uses `standard`, Python 3.12 and `All` platforms, with warnings failing the command. Type annotations describe subprocess text results, filesystem paths, HTTP callbacks, dynamic module interfaces and JSON boundaries. There is no blanket diagnostic suppression or baseline file. Commands may also be run separately with `python:format:check`, `python:lint` and `python:types`.

Biome 2.5.3 is pinned in pnpm and applies its recommended lint preset, formatting and import organization to `.js`, `.jsx`, `.mjs`, `.ts`, `.tsx`, `.css` and `.html`. Git attributes keep all seven frontend extensions in LF even with `core.autocrlf=true`. HTML formatting is explicitly enabled despite its experimental status; updates must preserve the formatting fixed point and a working Web build. `biome:check` checks without writing; `biome:fix` applies safe fixes. `js:fix` remains an alias, and `js:check` preserves the combined Biome, Web type and build checks. Build outputs (`dist`, `target`, `artifacts`), dependencies and local virtual environments are excluded.

The Web package pins TypeScript 7.0.2, retains strict checking and includes Vite's client asset declarations. `web:types` runs `tsc --noEmit`, `web:build` runs Vite, and `web:check` runs both exactly once. Biome does not replace TypeScript. VS Code uses the recommended `TypeScriptTeam.native-preview` extension and the workspace's installed TypeScript package; Biome owns formatting. Save formats only; apply lint fixes explicitly.

Pre-push checks Web types in the pushed commit snapshot, including a non-HEAD ref. It links only the installed dependency directories into the temporary snapshot (junctions on Windows), reads the snapshot's tsconfig and sources, and verifies the root/Web manifests, workspace definition and pnpm lock against the local checkout and installed lock. Installed direct Web package versions must also match. A mismatch stops with setup instructions: prepare a separate checkout of the pushed commit with `pnpm install --frozen-lockfile` and push from that checkout. Hooks never install dependencies or check current working sources in place of the pushed sources. Pre-commit remains a format/lint check; production builds run explicitly and in CI.

## Macro call formatting

`eden-fmt` uses the pinned official nightly rustfmt for ordinary Rust and adapts `json!` and `tokio::select!` bodies that rustfmt cannot parse as Rust expressions. It lowers the DSL, formats it, then lifts the result. Each formatter output must parse as Rust, including valid literal escapes inside opaque macro bodies; literal values and identifiers must survive every formatter pass and the complete pipeline. The pipeline repeats until it reaches a fixed point, or reports failure without writing the file. The CLI, editor stdin, hooks and CI all enter this pipeline. `pnpm rust:check` and `pnpm rust:fmt` cover the root and independent authors; do not run stable `cargo fmt` as a second formatter.

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

`pnpm rust:fmt` invokes `eden-fmt write`, which owns both ordinary Rust and macro formatting. Direct use is `cargo run --locked -p eden-fmt -- write path/to/file.rs`; `check` is read-only and `stdin` emits only a validated result.

## String literals

Prefer direct string literals, including long messages and format strings with implicit captures. The 100-column rustfmt target guides code layout; it is not a string-length limit. Official nightly rustfmt may introduce backslash continuations in ordinary strings. Raw, byte and unbreakable strings may still exceed the target.

Use `concat!(...)` when its members express meaningful groups, such as HTTP headers, JSONL records or sections of a test input. Split at those boundaries even when one member is long. A byte-oriented fixture can use a byte literal or `concat!(...).as_bytes()`; `concat!` itself does not accept byte string literals. Raw strings can make quoting clearer, but their physical line breaks and indentation belong to the value. Preserve the exact text, escapes and line endings when changing its source representation.

A direct format string supports implicit captures:

```rust
format!("round {n} of the retry budget")
```

If a format string needs macro-generated text, Rust requires explicit arguments for its named placeholders because it cannot capture through macro expansion:

```rust
format!(concat!("status: {status}\r\n", "content: {}\r\n"), body, status = status)
```

`eden-fmt` accepts existing and generated backslash continuations and compares decoded string values, byte strings and C strings. It preserves literal spelling inside unknown macros, `stringify!`, macro definitions and attributes: these contexts can observe tokens instead of values. Nested opaque macros keep that protection inside known value macros. The value-macro convention covers `json!`, `select!`, standard formatting/output/assertion macros, `vec!`, `concat!` and `matches!`; do not shadow these names with a macro that observes literal spelling.

The pinned official formatter can split an escape incorrectly or change whitespace inside a literal. Such output is rejected with a diagnostic before writing. Adjust that input using a raw literal, meaningful `concat!` groups, or a byte-preserving `include_str!`/`include_bytes!` fixture. The adapter does not repair arbitrary string syntax. Merge concat groups that exist only for the old formatter's width limitation into direct literals and let the formatter choose breaks; retain meaningful protocol or record boundaries.

`matches!` with guards and other unsupported macro grammars still need readable source: put spaces around operators and inside struct patterns, and use separate lines for long arguments and guards. For complicated guards, use an ordinary `match` expression when automatic layout is needed. A passing check does not prove every opaque macro was formatted. When fixing a skipped ordinary statement, perturb its ordinary syntax in a temporary copy and verify that the pipeline repairs or rejects it.

## Doc comments

A doc comment carries what the signature cannot. A parameter name, a return type and a struct field already state *what* something is; a comment that repeats them is negative value, because it is one more thing to keep true that no compiler checks. What has no other home is the decision behind the code — why a fallback exists, which invariant a caller can rely on, what a deliberate omission protects — and that is what the comment is for. `#[workspace.lints.rust] missing_docs = "deny"` makes the coverage requirement a build error; nothing can enforce that a comment is worth reading, so this section states the bar instead.

Every public item of the workspace crates carries a comment. A module carries a `//!` header stating the part it plays, and its public items each state their intent or their contract. An item that is not public is documented when its reason is not obvious from its own body. Where a subject has a model rather than a single item — an ownership rule, an exit-status rule, a lifecycle — the crate header states it once and the item comments stay short and local: [`eden-process`](../crates/eden-process/src/lib.rs) defines the exit-status model in its module header, and [`eden-cli`](../crates/eden-cli/src/cli.rs) shows the per-item register, where a comment explains why a payload without a level still prints and why a fallback reads a literal token.

A caller who can be broken by an invariant is owed that invariant in the documentation of the item they call. No doctest is used here: no example is compiled or run. Test modules, test helper files and `tests/contract-authors/*` are outside this standard — their documentation is their own README, and their comments are not built for readers of the library.

Two things this section requires are invisible to [`pnpm doc:check`](#checks-and-fixes), because rustdoc documents the reachable public surface and nothing else. A module header on a private module is required here and is not checked: a broken link in one is caught by neither rustdoc nor a compile. Neither is a link inside a test module. Write those by hand; the check is not why they are right.

A field or a variant is different from an item, because its own name and type already state what it is. `pub parent_id: Option<u64>` is complete without prose, and `MissingDependency { contract: String }` says what it carries. Such a group takes `#[allow(missing_docs)]` at the item with a word on why, which is a statement about that item and not a silence over the module — and a field carrying its own attribute needs that allow on the field, because it does not inherit the struct's. Prose is owed when the name and type are not the whole meaning: what a `None` means beyond absence, whether a zero is unknown or really zero, where an unqualified value is resolved, whether a position is stable once written, or that a variant exists for a reason the caller has to know — [`eden-process`](../crates/eden-process/src/lib.rs)'s `Stop` documents exactly the absent code its fields cannot distinguish, and [`Record`](../crates/eden-protocol/src/coding.rs)'s `sequence` says that a copy renumbers it, which its type never could.

The one escape is honesty. When no real intent can be stated for an item, it is not made public, or it is marked `#[doc(hidden)]` and left as internal implementation; the gate accepts `#[doc(hidden)]` in place of a comment. Writing a sentence that restates the name or the type to satisfy the lint is the failure this section exists to prevent, and it hides the more useful finding: an item nobody can explain is usually an item that should not have been exported. Do not ask a check to measure comment length or coverage percentage, because any such number is one an author can raise by padding.

## Rustfmt limitations

`rustfmt.toml` records edition/style edition 2024, `unstable_features = true` and `format_strings = true`. The adapter passes these settings and `--unstable-features` explicitly to the pinned binary, with a 100-column target and child-module traversal disabled; its own collector owns file traversal. The compiler remains stable. CLI width/edition overrides apply to the whole adapter pipeline. Format-on-save uses the same defaults as the repository checks. A passing check is not a universal line-length or skipped-region detector; raw strings, comments and unsupported macros may remain wider.

Four more limits are worth knowing before reading `eden-fmt`'s output as a verdict on style. A comment written where a lowered arm has no room for it — between a pattern and its `=`, between `=` and the future, between the future and `=>`, or between `=>` and the handler — is refused rather than moved, and so is a multi-argument closure used as a future or a handler. A piece rustfmt moved to its own line keeps the indentation the lowered form gave it, which can sit one level deeper than the lifted arm suggests. A refusal raised while lifting names a position in the intermediate lowercase text rather than in the author's file, although the file is still left untouched. Any macro named `select!` is treated as tokio's grammar, and rustfmt itself drops a redundant leading `|` from an or-pattern.

## Fixed Pi core reference corpus

The fixed Pi corpus is a manually invoked research tool, outside product Quality. `scripts/verify-pi-reference.mjs` imports actual Pi v0.85.1 modules at commit `d981de1229ef899957bbe968bc8dcda02a21f477` with a controlled model stream. Its nine cases cover images, line continuation, validated batch editing, complete shell output, template paths, skill discovery, Unicode/search scope, queue priority and usage reserve. It does not compare Eden outputs with Pi outputs. Historical reference evidence remains valid only for its recorded inputs; it is never reported as a fresh CI pass.

Eden's corresponding semantic scenarios continue through installed coding/context/workspace suites and native platform tests. Product evolution changes these contracts explicitly; the reference environment no longer blocks unrelated product changes. Invoke the reference again when researching a new capability or changing the reference baseline.

For a local reference run, clone that Pi commit into a separate directory, run `npm ci --ignore-scripts --no-audit --no-fund` there, then set `PI_REFERENCE_ROOT` to the checkout's absolute path and run `node scripts/prepare-pi-reference.mjs` followed by `node scripts/verify-pi-reference.mjs` from this repository. Preparation verifies the exact npm `pi-ai@0.85.1` archive integrity and extracts only the generated catalog JSON omitted by the source checkout. The probe verifies both tracked source and generated data. Pi may download its own ripgrep executable if none is installed. These are verification dependencies, not Eden runtime dependencies.

The reference summary, raw results and complete Shell fixture are written under `artifacts/pi-reference*`; the summary identifies the scope and inputs. This is an enumerated core corpus, not a full Pi CLI or real-provider equivalence claim. Eden retains its explicit strict-edit default, durable queues, resource snapshots and process cleanup guarantees.

### CI receipts and diagnostics

Download `scope-receipts`, `proof-receipts`, `static-receipts` or `native-receipts-<OS>-<ARCH>` for scope, equivalence, outcomes, timings and suite results. `gh run download <run-id> --pattern '*receipts*' --dir receipts` retrieves small records without archives. Native receipts include installed results, archive verification and `ci/hooks-selection.json`, which distinguishes `passed`, `not_selected` and `failed`. Full command logs and preparation details remain in `static-logs` and `native-diagnostics-<OS>-<ARCH>`; binaries are separate `native-archive-<OS>-<ARCH>` artifacts. Cache maintenance uses separate `cache-receipts-*` and `cache-diagnostics-*` artifacts and makes no acceptance claim. Failure uploads preserve available records without changing outcomes.
