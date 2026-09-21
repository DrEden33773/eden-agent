# Contributing to eden-agent

Use Rust 1.98.1 from rust-toolchain.toml to build; formatting uses the official nightly pinned in rustfmt-toolchain. Product code and the SDK are Apache-2.0; preserve third-party notices when redistributing dependencies.

- Use plain Cargo dependency requirements such as `"1.2.3"`, including path dependencies with a version. Do not use `"=1.2.3"` to freeze dependencies; commit Cargo.lock and use `--locked` for reproducible builds.
- Build with `cargo build --workspace --locked`.
- Set up development tools with Node.js 22+, `pnpm install --frozen-lockfile`, Python 3.12+/uv 0.9.7 with `uv sync --locked`, and `pnpm hooks:install` after cloning. This installs hooks only for this repository; see [development checks](docs/development-checks.md).
- Check with `pnpm markdown:check`, `pnpm python:check`, `pnpm js:check`, `pnpm rust:check`, `pnpm clippy:check`, `pnpm doc:check`, `cargo test --workspace --locked`, and `pnpm rust:test:authors`. The Rust commands include the root workspace and every independent author under `tests/contract-authors/*`; the workspace test command does not reach those authors, so run their tests explicitly.
- Format `json!` and `tokio::select!` bodies with `eden-fmt`: `pnpm rust:check` and `pnpm rust:fmt` run it, and [the macro call standard](docs/development-checks.md#macro-call-formatting) states the shape it enforces.
- Document why, not what: [the doc comment standard](docs/development-checks.md#doc-comments) states which code must be documented, what a comment has to carry, and why an item you cannot explain is made non-public or `#[doc(hidden)]` rather than filled with a sentence that restates it.
- Prefer direct string literals, including long format strings with implicit captures. Use `concat!` for meaningful text groups and fixtures for large fixed inputs; [string literals](docs/development-checks.md#string-literals) explains automatic continuations, value protection and explicit refusals.
- Hooks only check: pre-commit reads the staged snapshot; pre-push reads the pushed commits. Use `pnpm python:format`, `pnpm js:fix`, `pnpm markdown:fix`, `pnpm rust:fmt`, or explicit `pnpm clippy:fix` separately, review changes, then stage them. Do not add auto-fixing or auto-staging to hooks.
- For native ABI, routing, or lifecycle changes, run `python3 scripts/verify.py`; coding, storage, Provider or tool changes also require `python3 scripts/verify-coding.py`; see [native plugin contract](docs/native-plugins.md) for ownership and stop semantics. The verifier builds external authors after fixing the installed host, then exercises installed native libraries.
- Write behavior tests first for lifecycle and protocol changes. Use isolated test resources and observable cleanup barriers.
- Keep Rust framework types within their owning dynamic library. Document every unsafe boundary's ownership and threading requirements.
- Changes to main require a PR, independent review, resolved discussions, and passing Quality CI. Merge the verified PR head using squash; do not bypass branch rules.
- Keep Markdown prose paragraphs on one physical line. Public documentation must be sufficient for an independent clone and plugin author.
