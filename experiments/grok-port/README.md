# Grok-derived frontend with a native Eden adapter

This local S4 candidate runs the complete fixed Grok Build Rust frontend against a real Eden host. `eden-grok-adapter` is now a Rust workspace binary using `eden-tui-client`; Python only prepares the reference source, launches processes and drives verification. The default `eden` frontend has not been replaced, and the remaining S4 workflows are still being connected.

The reference is xai-org/grok-build commit `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. The original pager, render, textarea, prompt, scrollback, tool tracker and Presenter remain in use. The source patches select Eden services and connect the existing widgets to them; they do not start Grok's agent to execute tools.

## Run the candidate

From the product root, using the local `artifacts/s4-g2-session-host` installation:

```sh
python3 experiments/grok-port/launch.py --cwd /path/to/project
```

Use `--host /path/to/installed/bin/eden` for another Eden installation. The launcher saves each new history under the canonical project cwd’s `.eden/sessions/`; endpoint files and host logs stay under `artifacts/g1-grok-port/sessions/`. It uses the host's existing model and credentials, and prints the endpoint and history on exit. Detaching leaves accepted work and the host running. To attach an existing host or restore a stopped history:

```sh
python3 experiments/grok-port/run.py --endpoint /path/to/endpoint.json
python3 experiments/grok-port/launch.py --resume /path/to/history.jsonl --cwd /path/to/project
```

If the saved composition differs, ordinary execution remains blocked and the candidate opens a read-only view with the changed-package diagnostic. `/sessions` creates Sessions, selects a directory, shows saved identity/tags/owner/diagnostics, and opens or renames histories through their live owner. Its tree supports branch navigation and selected-ancestry forks; clone, recover, upgrade and migration require a reviewed copy plan. Delete checks the confirmed identity and writer lock. Stop waits for writer cleanup, and stopping the initial host hands management to a temporary in-memory host so resume remains available. Review the destination and preservation/loss notes before choosing **Create this copy and open it**. Migration leaves the source bytes untouched and refuses an existing destination. Matching histories still resume normally; no unconditional rebind occurs. Other execution-open failures, including saved configuration for a missing instance, retain their diagnostic and offer the same read-only path.

Ordinary sends while busy keep the draft instead of entering a private Grok queue. Ctrl+C cancels a running Eden operation immediately and preserves the composer draft. Repeated idle Ctrl+C exits the pager. Startup shows the Eden connection state; the terminal title and resume hint name Eden. Connection, token and Session identity failures exit with a diagnostic and an attach-again route before entering the pager. The older Python-backed experiment remains at commit `0fc0c70` and in the preserved local candidate directory.

## Connected workflows

| Entry | Actual owner and effect |
| --- | --- |
| Composer, completion, streaming, tool rows and Diff | Eden prompt/events/history drive the original Grok components. Tool identities include their run; finished-attempt late deltas are ignored. |
| Ctrl+C and reopen | Eden cancel/terminal controls actual cleanup. Loading uses Grok's replay barrier and never submits a new prompt. Cancelled partial output remains readable. |
| `/model <provider/model> [effort]`, `/effort <level>` | Changes only the current Session. Eden accepts off, minimal, low, medium, high, xhigh and max; the adapter maps Grok `none` to Eden `off`. The host resolves model compatibility and the footer shows requested → effective when they differ. |
| `/settings` → Use & save model | Sequentially changes the initiating Session and saves the new-session default through Eden. The result stays inside the settings modal; partial failure preserves the actual current model and reports that the default was not saved. |
| Display settings | Original Grok typed settings control local presentation. Their state is isolated in `.grok-port` beside the endpoint; it is not shared business configuration. |
| `/resume`, `/sessions` | Project histories and legacy launcher UUID histories are discovered using their recorded cwd. Explicit directory inspection can show other projects. Session forms provide new, tree/branch navigation, fork/clone/recover/upgrade/migrate previews, confirmed stopped-history deletion and live-host stop/resume. Incompatible bindings open read-only; source histories remain untouched. |
| `/rename <title>` | Updates the title after durable metadata commits, retaining its tags. Failure keeps the committed title. Saved-session renames route to an existing writer or exclusively open a stopped history. |
| `/auth`, `/login`, `/logout` | Select provider and its available API-key, browser/device OAuth, refresh or logout method. OAuth offers Open authorization page and Copy authorization URL, with browser/clipboard delivery feedback. Private fields never use the composer or prompt history. Escape cancels a pending login and joins its wait run. |
| `/config` | Opens the host-authored configuration form. Typed fields retain source, binding revision and secret-presence metadata; Validate, Preview and Apply use the shared Session transaction and private-input channel. These edits are Session overrides; new-session model defaults remain the separate Settings operation. |
| `/resources` | Shows the owning cwd, startup trust, loaded revision, instruction sources, skill/template paths and discovery diagnostics. Reload uses the existing host lifecycle; a failed reload retains the old inventory and displays the complete error in scrollable details. |
| `/skill:<name> [arguments]`, `/template:<name> [arguments]` | The host expands loaded resources before provider I/O and saves the resource revision and prepared input. Skills use Eden's qualified identity. Templates also offer bare names where the pager has no existing command; qualification keeps colliding names callable. |
| Context footer | Shows the effective request estimate and effective model window, with `~` for estimates and `?` for unknown values. Uses the coding loop’s shared estimate, including tool schemas; model/context edits, compaction and Session changes refresh it. Character cells shrink on narrow terminals. Settings → Context footer toggles visibility for this frontend process. |
| `/usage` | Shows the latest reported request counters from persisted Eden usage, preserving unknown counters as `?`. |
| `!command` | Uses the independent Eden user-shell operation and renders its result as user-owned tool output. It does not request a model response. |
| `/compact`, prompt history | Use Eden's compaction and actual saved user prompts. These paths are connected; the complete compaction/context workflow matrix is still pending. |
| `/capabilities` | Lists the connected coverage, pending integrations and bundled-backend gaps. |

Use Tab/Shift+Tab or arrow keys to navigate management fields and actions, Enter to activate, and Escape to close. Boolean and choice fields also accept Left/Right or Space. JSON/list fields accept JSON; Ctrl+U clears a configuration override and Ctrl+R restores inheritance. PgUp/PgDn scroll long preview details. Secret fields are always masked, and editing state is discarded on close.

Prompt acceptance is acknowledged as soon as the host supplies the run identity, independently of the first provider event and the terminal result. Notifications retain prompt/run/attempt identity; uncertain acceptance is reconciled through the original request receipt without resubmitting. Cancellation is bound to the originating prompt, including cancellation while acceptance is pending. Running-session attachments adopt a Session/run identity, retain cancellation ownership and receive terminal notifications after replay; an unavailable original request identity remains unknown. The original 120-second unacknowledged-prompt guard remains enabled.

Slash commands, the command palette, registered shortcuts and settings expose the connected subset. Unavailable voice/task/cloud commands are not offered. Manually typing a disconnected command produces an explanation and does not send it to the model. Existing-but-unconnected capabilities remain required work; their absence is not a backend exemption.

## Build from fixed source

Build and install an Eden host using the normal project instructions. For an explicit local installation from debug artifacts:

```sh
cargo build --workspace --locked
python3 scripts/install.py artifacts/s4-g2-session-host --profile debug
```

Obtain `https://github.com/xai-org/grok-build.git` at the fixed commit above, or extract a `git archive` of that commit into `artifacts/g1-grok-source`. Retain its LICENSE and THIRD-PARTY-NOTICES. The private integration evidence contains the original archive; source recovery does not depend on a temporary checkout.

```sh
python3 experiments/grok-port/build.py --source artifacts/g1-grok-source --protoc /path/to/protoc
python3 experiments/grok-port/launch.py --host artifacts/s4-g2-session-host/bin/eden --cwd /path/to/project
```

Both binaries build with Rust 1.98.1 and their locked dependencies. Protobuf generation was verified with protoc 31.1. The reference's locked git dependencies must be available to Cargo. Build output lives in `artifacts/g1-native-target` and the runnable candidate in `artifacts/g1-native-port`; the script includes both projects' notices. This is a local candidate build, not a release installation or a change to the default CLI.

## Verification

```sh
cargo test -p eden-grok-adapter --locked
python3 experiments/grok-port/verify.py --management --thinking --ack-lifecycle --draft-cancel --busy-send --output artifacts/g1-native-port/verification
python3 experiments/grok-port/verify_workflows.py --installation artifacts/s4-g2-session-host --output artifacts/g1-native-port/workflows
python3 experiments/grok-port/verify_workflows.py --case thinking --output artifacts/g1-native-port/thinking-wire
python3 experiments/grok-port/verify_lifecycle.py --output artifacts/g1-native-port/lifecycle
python3 experiments/grok-port/verify_lifecycle.py --long --output artifacts/g1-native-port/long-stream
python3 experiments/grok-port/verify_idle.py --output artifacts/g1-native-port/idle --assert-idle
python3 experiments/grok-port/verify_startup.py --output artifacts/g1-native-port/startup
python3 experiments/grok-port/verify_resources.py --output artifacts/g1-native-port/resources
```

The POSIX PTY probes accept `--installation artifacts/s4-g2-session-host`, isolated configuration, fixture credentials and a loopback SSE provider. It performs real filesystem/shell work, checks model scope and injected default-save failure, observes the endpoint opened by the picker, verifies mutations reach that host, and preserves the host's public provider failure diagnostic. Test-created hosts are shut down. The workflows probe reuses the independent `configuration-forms` native author from `target/verification/seeds/authors/configuration-forms`; `python3 scripts/verify.py` prepares that fixture. Authentication uses controlled loopback OAuth issuers and fixture keys. These receipts are distinct from external-account, manual terminal and native Windows/macOS evidence.

For styled captures, copy `decode.rs` to the reference's `crates/codegen/ptyctl/examples/eden-decode.rs`, build it with `cargo +1.98.1 build --locked -p ptyctl --example eden-decode`, and place the binary at `artifacts/g1-grok-port/eden-decode`. It uses the reference's Alacritty terminal model to decode the raw PTY output. `screens.py` converts that cell data to SVG.

## Remaining integration

Attachments/images, resource package management, explicit steering/follow-up queues, complete session tree/fork/clone management, context editing and fixed references, router/notes/cache-warmer, presentation forms and slots, five native UI replacement roles, reading/export/share and updates are not yet connected to this frontend. They remain available through the existing Eden implementation and are required follow-up work. Full retry/event-lag combinations, complete tool metadata and artifact browsing, history reasoning/attachments, global display preferences, branding and background policy also remain to be completed and checked.

The native C ABI is unchanged and contains no ratatui types. This candidate does not load those UI replacements; it must not be used as evidence that the five roles work in the new frontend. The complete Grok dependency tree is still compiled, while connected execution/model/session operations use Eden. Local display preferences continue to use the isolated upstream configuration machinery.

## Attribution

Grok Build is copyright xAI and Apache-2.0 licensed. The full reference source and candidate retain its LICENSE and THIRD-PARTY-NOTICES, including upstream assets and transitive dependency notices. Eden's modifications include transport and correlated acceptance, service/capability routing, protected history recovery, private management modals, model scope and thinking mapping, run-first cancellation, native projection, launch/build tools and verification. Eden startup, window titles, Session-ready notifications and attach hints use Eden branding. Remaining global branding/display policy is part of the unfinished S4 review; the reference source and notices retain their attribution and do not imply affiliation with xAI.

Session and footer acceptance use `verify_sessions.py` and `verify_context_footer.py` with `--installation artifacts/s4-g2-session-host --output /path/to/new-evidence`. The launcher regression is `python3 -m unittest discover -s experiments/grok-port -p test_launch.py`. These local checks do not close the remaining S4 integration or platform matrix.
