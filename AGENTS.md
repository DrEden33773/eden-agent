# Contributing to eden-agent

Use Rust 1.89.0 from rust-toolchain.toml. Product code and the SDK are Apache-2.0; preserve third-party notices when redistributing dependencies.

- Build with `cargo build --workspace --locked`.
- Check with `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked`.
- For native ABI, routing, or lifecycle changes, run `python3 scripts/verify.py`; see [native plugin contract](docs/native-plugins.md) for ownership and stop semantics. The verifier builds external authors after fixing the installed host, then exercises installed native libraries.
- Write behavior tests first for lifecycle and protocol changes. Use isolated test resources and observable cleanup barriers.
- Keep Rust framework types within their owning dynamic library. Document every unsafe boundary's ownership and threading requirements.
- Changes to main require a PR, independent review, resolved discussions, and passing Quality CI. Merge the verified PR head using squash; do not bypass branch rules.
- Keep Markdown prose paragraphs on one physical line. Public documentation must be sufficient for an independent clone and plugin author.
