# Contributing to eden-agent

Use Rust 1.98.1 from rust-toolchain.toml. Product code and the SDK are Apache-2.0; preserve third-party notices when redistributing dependencies.

- Build with `cargo build --workspace --locked`.
- Set up development tools with Node.js 22+, `pnpm install --frozen-lockfile`, and `pnpm hooks:install` after cloning. This installs hooks only for this repository; see [development checks](docs/development-checks.md).
- Check with `pnpm markdown:check`, `pnpm rust:check`, `pnpm clippy:check`, and `cargo test --workspace --locked`. The Rust commands include the root workspace and every independent author under `tests/contract-authors/*`.
- Hooks only check: pre-commit reads the staged snapshot; pre-push reads the pushed commits. Use `pnpm markdown:fix`, `pnpm rust:fmt`, or explicit `pnpm clippy:fix` separately, review changes, then stage them. Do not add auto-fixing or auto-staging to hooks.
- For native ABI, routing, or lifecycle changes, run `python3 scripts/verify.py`; coding, storage, Provider or tool changes also require `python3 scripts/verify-coding.py`; see [native plugin contract](docs/native-plugins.md) for ownership and stop semantics. The verifier builds external authors after fixing the installed host, then exercises installed native libraries.
- Write behavior tests first for lifecycle and protocol changes. Use isolated test resources and observable cleanup barriers.
- Keep Rust framework types within their owning dynamic library. Document every unsafe boundary's ownership and threading requirements.
- Changes to main require a PR, independent review, resolved discussions, and passing Quality CI. Merge the verified PR head using squash; do not bypass branch rules.
- Keep Markdown prose paragraphs on one physical line. Public documentation must be sufficient for an independent clone and plugin author.
