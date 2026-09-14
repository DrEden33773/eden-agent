# eden-agent

A Rust coding agent built from replaceable native plugins.

This development release runs a controlled in-memory agent loop through native context, provider and tool roles. External Rust authors can independently replace the loop and context without rebuilding the host. The included provider and echo tool are deterministic; they do not call a model service or modify your files.

## Build and run

Install Rust via rustup and Python 3. The repository selects Rust 1.89.0; Python is only used for installation assembly and verification.

```sh
cargo build --workspace --locked
python3 scripts/install.py artifacts/install
artifacts/install/bin/eden --print hello
artifacts/install/bin/eden --json hello
```

On Windows use `python` and `artifacts/install/bin/eden.exe`. The expected print result is `standard:hello => echo[standard:hello]`. You can invoke the installed executable from another working directory: its default composition resolves relative to the executable. Use `--composition PATH` to explicitly select another local composition.

The installed directory contains `bin/eden`, `composition.json`, versioned native packages under `plugins/`, and license notices. Move or archive that directory as a unit. There is no global installation step or implicit download at startup.

## Rust embedding

```sh
cargo run -p eden-agent --example embedded -- artifacts/install/composition.json
```

The [embedded example](crates/eden-agent/examples/embedded.rs) uses the same `Session::open`, `submit`, `wait` and `shutdown` API as the CLI. Accepted submissions have a run identity; final results are retained for `inspect` even when a waiting receiver is dropped. `events_after` reads ordered in-memory events. Await `shutdown` before dropping your Tokio runtime.

## Native plugin authors

Read the [author guide](docs/plugin-authoring.md) and [native contract](docs/native-plugins.md). The public SDK is `crates/eden-plugin-sdk`; it re-exports protocol types. [Loop A](tests/contract-authors/loop-a) and [Context B](tests/contract-authors/context-b) are independently built author examples. First-party roles use the same SDK and loader.

The verifier first builds and fixes the installed host, copies only the SDK/protocol into a separate author tree, then builds and installs external libraries. It exercises default and mixed role combinations from a different working directory and compares the host bytes before and after.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/verify.py
```

The native CI matrix runs these checks on Linux x86_64, Windows x86_64 and the actual macOS runner architecture. Its artifacts contain an installation archive and machine-readable verification results with the tested target and commit. These checks cover native loading and lifecycle, not interactive terminal behavior.

## Contributions and license

Changes enter `main` through a PR with independent review and passing Quality checks, using squash merge. See [AGENTS.md](AGENTS.md) for development commands.

Product code and SDK: [Apache-2.0](LICENSE). Dependencies retain their [third-party notices](THIRD_PARTY_NOTICES.md).
