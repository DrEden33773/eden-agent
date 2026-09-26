# eden-agent

A Rust coding agent built from replaceable native plugins.

This development release executes coding tasks through native loop, context, provider, tool and history roles. The default combination provides model catalogs, private API-key management, OpenAI Responses, Chat Completions, Anthropic Messages and cloud model protocols, alongside read/write/edit/bash and local JSONL history. Configure model access and credentials through [Models and API keys](docs/models.md); native roles are independently replaceable without rebuilding the host.

## Build and run

Install Rust via rustup and Python 3. The repository selects Rust 1.98.1; Python is only used for installation assembly and verification.

```sh
cargo build --workspace --locked
python3 scripts/install.py artifacts/install
artifacts/install/bin/eden --cwd /path/to/project --session /path/to/task.jsonl --print 'Inspect the project and run its tests.'
artifacts/install/bin/eden --history /path/to/task.jsonl
```

On Windows use `python` and `artifacts/install/bin/eden.exe`. Set an exact model in `OPENAI_MODEL` and a bearer credential in `OPENAI_API_KEY`, or copy [`.env.example`](.env.example) for DeepSeek and load it explicitly with `--env-file PATH`; see [coding sessions](docs/coding.md) for configuration, attachments, resume, queues and tool behavior. You can invoke the installed executable from another working directory: its default composition resolves relative to the executable. Use `--composition PATH` to explicitly select another local composition.

The installed directory contains `bin/eden`, `composition.json`, versioned native packages under `plugins/`, and license notices. Move or archive that directory as a unit. There is no global installation step or implicit download at startup.

Ordinary JSON results use two-space indentation and a trailing newline, including when piped or redirected. Terminal stdout uses syntax colors automatically; `--color never` disables color without removing indentation, and `--color always` forces color. Automatic color respects `NO_COLOR` and `TERM=dumb`, with `CLICOLOR_FORCE` taking precedence. Use `--json` for uncolored compact machine output, even with `--color always`; run and router events remain JSONL. History inspection, `--history`, and session tree records always remain uncolored JSONL. Session copy operations with `--apply` print the plan followed by the execution result as two JSON documents (one line each with `--json`). History export files retain their storage format.

Use `eden help`, `eden help models` or `eden help models select` to explore commands without loading plugins. `-h` and `--help` remain available at every command level. Building only the CLI with `cargo run --bin eden` does not assemble an installation: its default composition would be `target/composition.json`. After the build and install steps above, you can also run `cargo run --locked --bin eden -- --composition artifacts/install/composition.json models list`. Explicit composition and history paths are relative to the invoking process directory, independently of `--cwd`.

## Rust embedding

```sh
cargo run -p eden-agent --example embedded -- artifacts/install/composition.json
```

The [embedded example](crates/eden-agent/examples/embedded.rs) uses the same `Session::open`, `submit`, `wait` and `shutdown` API as the CLI. Accepted submissions have a run identity; final results are retained for `inspect` even when a waiting receiver is dropped. `events_after` reads ordered in-memory events. Await `shutdown` before dropping your Tokio runtime.

Configuration queries, validation, previews and local application are available through the same SDK, [CLI and RPC management interface](docs/configuration.md). A configuration operation has its own identity, waits for affected foreground runs by default, and preserves unrelated instances.

## Native plugin authors

Read the [author guide](docs/plugin-authoring.md) and [native contract](docs/native-plugins.md). The public SDK is `crates/eden-plugin-sdk`; it re-exports protocol types. [Loop A](tests/contract-authors/loop-a) and [Context B](tests/contract-authors/context-b) are independently built author examples. First-party roles use the same SDK and loader.

The verifier first builds and fixes the installed host, copies only the SDK/protocol into a separate author tree, then builds and installs external libraries. It exercises default and mixed role combinations from a different working directory and compares the host bytes before and after.

```sh
pnpm markdown:check
pnpm rust:check
pnpm clippy:check
cargo test --workspace --locked
pnpm rust:test:authors
python3 scripts/verify.py
python3 scripts/verify-coding.py
```

The native CI matrix runs these checks on Linux x86_64, Windows x86_64 and the actual macOS runner architecture. The test targets whose behavior differs per operating system — process ownership, link creation, path scope and history locking — run on all three; the remaining workspace tests run on Linux, while Clippy and the acceptance suites still compile and load every plugin on every runner. Its artifacts contain an installation archive and machine-readable coding/native verification results with the tested target and commit. These checks cover native loading and lifecycle, not interactive terminal behavior.

## Contributions and license

Install Node.js 22+ and pnpm 10.32.1 for development checks, then run `pnpm install --frozen-lockfile` and `pnpm hooks:install`. Each clone configures its own hooks. Node/pnpm are development dependencies; the built application remains a Rust executable. See [development checks](docs/development-checks.md) for staged-snapshot checks, independent author coverage and explicit fix commands.

Changes enter `main` through a PR with independent review and passing Quality checks, using squash merge. See [AGENTS.md](AGENTS.md) for development commands.

Product code and SDK: [Apache-2.0](LICENSE). Dependencies retain their [third-party notices](THIRD_PARTY_NOTICES.md).

See [resources, trust and package composition](docs/workspace.md) for Skills, templates, FFF search, package commands and independent service contributions.

See [Models and API keys](docs/models.md) for catalog refresh, private credentials, model selection and cross-protocol continuation.

Account login, device authorization, private callback input and token refresh are available through the CLI and SDK; see [account authentication](docs/authentication.md) and [model routing](docs/models.md).

Local model discovery, search, server-side download, load/unload and cancellation use the replaceable ModelManager role; see [llama.cpp router](docs/router.md).

Rust consumers can use the [embedding API](docs/embedding.md); subprocess clients use the [stdio RPC protocol](docs/rpc.md).

See [export, explicit sharing and startup maintenance](docs/delivery.md) for reading snapshots and delivery services, and [updates](docs/updates.md) for complete-installation transactions.

The [presentation contract](docs/presentation.md) provides one semantic Rust plugin view for minimal Ratatui and React/Spectrum adapters through an explicit local shared host, plus plugin-free read-only access to saved semantic views.
