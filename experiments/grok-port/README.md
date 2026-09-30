# Grok-derived frontend with a native Eden adapter

This local G1 candidate runs the complete fixed Grok Build Rust frontend against a real Eden host. `eden-grok-adapter` is now a Rust workspace binary using `eden-tui-client`; Python only prepares the reference source, launches processes and drives verification. The default `eden` frontend has not been replaced, and the remaining S4 workflows are still being connected.

The reference is xai-org/grok-build commit `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. The original pager, render, textarea, prompt, scrollback, tool tracker and Presenter remain in use. The source patches select Eden services and connect the existing widgets to them; they do not start Grok's agent to execute tools.

## Run the candidate

From the product root, using the preserved local host installation:

```sh
python3 experiments/grok-port/launch.py --cwd /path/to/project
```

Use `--host /path/to/installed/bin/eden` for another Eden installation. The launcher creates a persistent task under `artifacts/g1-grok-port/sessions/`, uses the host's existing model and credentials, and prints the endpoint and history on exit. Detaching leaves accepted work and the host running. To attach an existing host or restore a stopped history:

```sh
python3 experiments/grok-port/run.py --endpoint /path/to/endpoint.json
python3 experiments/grok-port/launch.py --resume /path/to/history.jsonl --cwd /path/to/project
```

Ordinary sends while busy keep the draft instead of entering a private Grok queue. Ctrl+C cancels a running Eden operation immediately and preserves the composer draft. Repeated idle Ctrl+C exits the pager. The original Grok resume hint may still be printed; use the launcher's final `Attach:` command for Eden. The older Python-backed experiment remains at commit `0fc0c70` and in the preserved local candidate directory.

## Connected workflows

| Entry | Actual owner and effect |
| --- | --- |
| Composer, completion, streaming, tool rows and Diff | Eden prompt/events/history drive the original Grok components. Tool identities include their run; finished-attempt late deltas are ignored. |
| Ctrl+C and reopen | Eden cancel/terminal controls actual cleanup. Loading uses Grok's replay barrier and never submits a new prompt. Cancelled partial output remains readable. |
| `/model <provider/model>` | Changes the current Eden Session only. Canonical provider/model labels distinguish otherwise identical catalog names. |
| `/settings` → Use & save model | Sequentially changes the initiating Session and saves the new-session default through Eden. The result stays inside the settings modal; partial failure preserves the actual current model and reports that the default was not saved. |
| Display settings | Original Grok typed settings control local presentation. Their state is isolated in `.grok-port` beside the endpoint; it is not shared business configuration. |
| `/resume` | The original picker lists saved Eden histories from the current project's `.eden/sessions` and the endpoint directory. Selection calls the host's `/manage/open`; subsequent actions use the selected host. |
| `/usage` | Shows the latest reported request counters from persisted Eden usage, preserving unknown counters as `?`. |
| `!command` | Uses the independent Eden user-shell operation and renders its result as user-owned tool output. It does not request a model response. |
| `/compact`, prompt history | Use Eden's compaction and actual saved user prompts. These paths are connected; the complete compaction/context workflow matrix is still pending. |
| `/capabilities` | Lists the connected coverage, pending integrations and bundled-backend gaps. |

Slash commands, the command palette, registered shortcuts and settings expose the connected subset. Unavailable voice/task/cloud commands are not offered. Manually typing a disconnected command produces an explanation and does not send it to the model. Existing-but-unconnected capabilities remain required work; their absence is not a backend exemption.

## Build from fixed source

Build and install an Eden host using the normal project instructions. For an explicit local installation from debug artifacts:

```sh
cargo build --workspace --locked
python3 scripts/install.py artifacts/g1-host --profile debug
```

Obtain `https://github.com/xai-org/grok-build.git` at the fixed commit above, or extract a `git archive` of that commit into `artifacts/g1-grok-source`. Retain its LICENSE and THIRD-PARTY-NOTICES. The private integration evidence contains the original archive; source recovery does not depend on a temporary checkout.

```sh
python3 experiments/grok-port/build.py --source artifacts/g1-grok-source --protoc /path/to/protoc
python3 experiments/grok-port/launch.py --host artifacts/g1-host/bin/eden --cwd /path/to/project
```

Both binaries build with Rust 1.98.1 and their locked dependencies. Protobuf generation was verified with protoc 31.1. The reference's locked git dependencies must be available to Cargo. Build output lives in `artifacts/g1-native-target` and the runnable candidate in `artifacts/g1-native-port`; the script includes both projects' notices. This is a local candidate build, not a release installation or a change to the default CLI.

## Verification

```sh
cargo test -p eden-grok-adapter --locked
python3 experiments/grok-port/verify.py --management --draft-cancel --busy-send --output artifacts/g1-native-port/verification
```

The POSIX PTY probe uses the preserved `artifacts/r1-candidate` host, isolated configuration, fixture credentials and a loopback SSE provider. It performs real filesystem/shell work, checks model scope and injected default-save failure, observes the endpoint opened by the picker, verifies mutations reach that host, and preserves the host's public provider failure diagnostic. Test-created hosts are shut down. These receipts are distinct from real-service, manual terminal and native Windows/macOS evidence.

For styled captures, copy `decode.rs` to the reference's `crates/codegen/ptyctl/examples/eden-decode.rs`, build it with `cargo +1.98.1 build --locked -p ptyctl --example eden-decode`, and place the binary at `artifacts/g1-grok-port/eden-decode`. It uses the reference's Alacritty terminal model to decode the raw PTY output. `screens.py` converts that cell data to SVG.

## Remaining integration

Provider API-key/OAuth forms and private input, generic business configuration, attachments/resources/skills, explicit steering/follow-up queues, complete session directory/tree/fork/clone management, context editing and fixed references, router/notes/cache-warmer, presentation forms and slots, five native UI replacement roles, reading/export/share and updates are not yet connected to this frontend. They remain available through the existing Eden implementation and are required follow-up work. Full retry/event-lag combinations, complete tool metadata and artifact browsing, history reasoning/attachments, global display preferences, branding and background policy also remain to be completed and checked.

The native C ABI is unchanged and contains no ratatui types. This candidate does not load those UI replacements; it must not be used as evidence that the five roles work in the new frontend. The complete Grok dependency tree is still compiled, while connected execution/model/session operations use Eden. Local display preferences continue to use the isolated upstream configuration machinery.

## Attribution

Grok Build is copyright xAI and Apache-2.0 licensed. The full reference source and candidate retain its LICENSE and THIRD-PARTY-NOTICES, including upstream assets and transitive dependency notices. Eden's modifications are the transport, service/capability routing, explicit-session restore hooks, model scope and feedback handling, run-first cancellation policy, native protocol projection, launch/build tools and verification. The inherited branding is still being adapted and does not imply affiliation or endorsement by xAI.
