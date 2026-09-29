# Whole-pager Eden transplant experiment

This executable experiment connects the complete Grok Build Rust pager to a real Eden Session. It is an integration probe, not the default Eden frontend or a completed replacement. The existing host, history, trust policy and native UI contracts are unchanged. The new pager does not yet consume all of them.

The reference is xai-org/grok-build commit `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. `prepare.py` adds an ACP process transport and replaces Grok's local resume lookup with explicit Eden endpoint identity. The original pager, render, textarea, prompt, scrollback, tool tracker and event loop remain in use. `adapter.py` maps Eden's authenticated loopback API, incremental snapshots, commits and events to this tracker. It never invokes Grok's agent to execute tools.

## Run the local experiment

With the locally built `artifacts/g1-grok-port/eden-grok` and preserved `artifacts/r1-candidate` installation, run from the product root:

```sh
python3 experiments/grok-port/launch.py --cwd /path/to/project
```

The launcher creates a persistent isolated task under `artifacts/g1-grok-port/sessions/`, uses Eden's existing model and credential configuration, and prints the endpoint and history on exit. Detaching leaves the Eden host running. To attach an existing explicit host:

```sh
python3 experiments/grok-port/run.py --endpoint /path/to/endpoint.json
```

To restore a stopped Eden history, use `launch.py --resume /path/to/history.jsonl --cwd /path/to/project`. Startup replays history through Grok's load barrier without executing a prompt. Ctrl+C cancels the active Eden run; the ACP prompt settles only after Eden's actual terminal result. Repeated idle Ctrl+C exits the pager. `/eden-status` reads the actual host state and is advertised to Grok's command completion.

## Reproduce the build

Obtain the fixed source with `git clone https://github.com/xai-org/grok-build.git`, check out the commit above, and retain its LICENSE and THIRD-PARTY-NOTICES. Alternatively extract a `git archive` of that commit into `artifacts/g1-grok-source`. The private integration evidence retains that archive as well as the patches here, so recovery does not depend on a temporary checkout.

```sh
python3 experiments/grok-port/prepare.py artifacts/g1-grok-source
```

Build in the extracted source directory with its locked dependencies and pinned Rust 1.94.0 reference toolchain: `cargo build --locked -p xai-grok-pager-bin --bin xai-grok-pager`. This is an isolated upstream dependency trial; normal Eden builds continue to use Rust 1.98.1. Protobuf code generation requires `protoc`. The upstream lockfile contains its pinned git dependencies. Copy the resulting binary to `artifacts/g1-grok-port/eden-grok` and its license files into that directory. A final integrated Eden crate and toolchain build remain to be done.

For accurate PTY captures, copy `decode.rs` to the reference's `crates/codegen/ptyctl/examples/eden-decode.rs`, build with `cargo build --locked -p ptyctl --example eden-decode`, and copy that executable to `artifacts/g1-grok-port/eden-decode`. It uses the same Alacritty terminal model as the official reference harness; the earlier Eden text-only driver does not fully decode OSC title sequences.

```sh
python3 -m unittest discover -s experiments/grok-port -p 'test_*.py'
python3 experiments/grok-port/verify.py --output artifacts/g1-grok-port/verification
```

The verification starts an isolated Eden host and a loopback SSE provider, uses fixture credentials, and executes real filesystem and shell tools. It exercises completion, streaming, cancellation, continued interaction, six tool calls, resize, history reopening, and cancellation of a shell process tree. Captures come from the PTY; successful host operations alone do not establish visual parity.

## Known integration gaps

Grok's settings and several commands call its own config/session services directly, outside ACP. Their presence in a menu does not mean they manage Eden. Model defaults, authentication challenges, session management, queue/steering, shell mode, context editing, resources, plugins, exports and updates are not connected. Native Editor/Renderer/Theme/Overlay/TerminalFrontend replacement is preserved in the existing Eden installation but is not loaded by this experiment. The default interface has not been replaced.

The adapter is deliberately kept here as an inspectable experiment until those ownership boundaries are replaced. It also needs attempt-scoped retry/reconnect handling, usage/status-line mapping, complete tool metadata including batch Diff line origins, attachment/presentation handling, and Eden branding/default-background integration. Upstream local settings still belong to the isolated `.grok-port` directory beside the endpoint. Use an isolated project for exploration of unconnected Grok controls. The launcher disables Grok telemetry, feedback and updater channels and does not export Eden credentials to Grok.

## Attribution

Grok Build is copyright xAI and distributed under Apache-2.0. The reference archive and binary retain its LICENSE and THIRD-PARTY-NOTICES, including third-party dependencies. The changes made here are the Eden transport, explicit-session restore hooks, protocol projection, launchers and verification/decoding entrypoints. They do not imply affiliation or endorsement by xAI. The upstream branding visible in this experiment is an outstanding product integration task.
