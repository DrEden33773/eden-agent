# Embedding sessions

`eden-agent` provides the shared session used by print CLI, stdio [RPC](rpc.md), and Rust callers. Runtime work belongs to a `Session`; await `shutdown()` before dropping its Tokio runtime. Cloning a handle or dropping an event waiter does not transfer or cancel that ownership.

## Opening and contribution identity

Use `Session::open_with_workspace` for an installed composition file, or `embedded::Embedded::new(composition, base)` for an explicit `eden_protocol::Composition`. `base` resolves library paths; `SessionOptions.cwd` independently fixes the working directory. `history: None` creates no session file. Two sessions can have different working directories and different providers in the same process without changing process cwd.

`Embedded::package(package, identity)` accepts an `eden_plugin_sdk::Package` running on the caller's runtime. It selects that package's registered roles; a selected role can replace a native implementation. Package names must be unique. The kernel mounts these contributions with the same operation scopes, cancellation, role routing, and instance finalization as native packages. Calls between libraries still contain only versioned serialized data. Retained role proxies reject work after their generation closes.

The caller supplies a stable compatibility identity for each in-process contribution. Saved composition bindings retain those identities and descriptors, never closures or serialized runtime objects. Reopening requires the same contributions again. Missing or incompatible implementations refuse continuation; `eden_agent::history::read(path)` still reads the original public history without loading them. An explicit composition switch is the existing recovery operation and cannot silently substitute a missing contribution.

## Tools and resources

`Embedded::tool` registers a `ToolDefinition`, identity, `ToolOptions`, preprocessing closure, and asynchronous execution closure. The preprocessing result is validated against the published JSON Schema before execution. External schema retrieval is disabled. The execution closure receives `ToolRequest` and `CallContext`: call ID and cwd, structured results, `emit` for increments, and `scope` for managed children and cleanup. System guidance and guidelines are included in the advertised description. The default tool registry merges contributions, rejects duplicate names, and applies exclusions, read-only eligibility, and explicit tool selection to both the advertised schema and actual route.

`ToolDefinition.execution` defaults to `sequential`. Consecutive `parallel` calls execute concurrently; sequential calls form barriers. The default loop uses the final tool name after before-tool hooks and commits results in original call order. Cancellation drains every admitted service bridge and registered cleanup before settlement.

`Embedded::resources(identity, absolute_base, provider)` accepts a provider for the public resource request/reply protocol. Its first request is `Reload`; later `Snapshot` reads the frozen result. A successful reload replaces the whole snapshot and advances its revision. A failed reload leaves the previous snapshot available. Supply context instructions, replacement/append system text, skill/template metadata, source paths and diagnostics in the snapshot. Relative metadata paths resolve from the explicit base without requiring files to exist. The provider handles `Expand` and `Skill` against its frozen contents and must return the current revision. Resources are captured at the loop's input boundary, so reload does not rewrite an already accepted model request.

The standalone consumer in `tests/contract-authors/embedded-client` depends only on the public session and plugin SDK crates. Installed verification compiles it after freezing the host. It exercises a caller tool through the native default loop, model-visible resources, cancellation cleanup, two independent working directories, failed reload, persistent queue withdrawal and reopening.

## Operations and admission

`submit_blocks` and `resume` accept a model run and return its run ID. `wait` returns the terminal after cleanup; `cancel` only signals cancellation. State, tools, commands, resources, models, history and queue queries can be read without waiting for a model run to finish. `enqueue`, `withdraw_queue`, and `configure_queue` remain available during a model run. Queue mode changes are ordered with the next delivery transaction and do not rewrite already delivered entries. Withdrawal affects waiting/returned entries on the current branch; delivered entries must first return through cancellation. A withdrawal is recorded independently of model consumption and cannot reappear after reopening.

`control` changes automatic compaction or retry policy for this live package instance. `StopRetry` ends the current backoff with its original provider error and does not cancel other work. These switches do not edit saved workspace settings. History navigation, compaction, reload, metadata and composition changes require idle admission.

`user_shell(command, shell, exclude_from_context)` starts an explicit `bash` or `powershell` operation, independently of the model's tool allowlist. It uses the session cwd, the selected user-shell service and its optional before-user-shell hook. `user_shell_output` events contain stream names and byte chunks; decode each stream incrementally. `cancel_shell` signals only that operation; `wait` includes process-tree cleanup. A cancelled terminal keeps captured output and artifact references in `partial_result`; its outcome remains cancelled, and the same retained result enters the shell history record. Public `user_shell` records retain the command and result; excluded records stay in history while being omitted from model projection. A shell can start during a model run, but defers its history record until that run settles. Further model/history operations wait for the shell to settle, preserving tool-call/result order.

Contributed commands have independent run IDs and can execute while the model is active. Their cancellation and shutdown belong to the session. History-management admission is excluded while commands or shell operations are outstanding.

## Observation and semantic interaction

`read_events(sequence)` returns ordered events after a session-local cursor. Multiple readers have independent cursors. A closed, drained stream returns an empty vector; closing an idle session wakes readers. The retained window is limited to 8192 events and approximately 8 MiB of payloads. An expired cursor returns `Lagged`, so clients can read durable history or reconnect to a new observation window explicitly. Cursors do not resume across processes. The compatibility `events_after` method reports a synthetic `events_lagged` event instead of silently omitting observations. `events()` returns the retained window, not an unlimited history archive.

Call `set_interactions(true)` when a caller will handle semantic extension interaction. Observe `interaction_requested`, then call `respond_interaction(interaction_id, value)`. Supported dialogs are select, confirm, input and editor; null dismisses a dialog. Select replies must match an offered string, confirm replies are booleans, and input/editor replies are strings. Duplicate, expired and malformed replies fail without answering another request. Disabling interaction detaches the handler and cancels outstanding dialogs. Timeout, invoking-run cancellation and shutdown also release waiters.

Native and caller-runtime extensions use `eden_protocol::interaction::HOST` with `Interaction::Request` or `Interaction::Notify`. Notification kinds are notify, status, widget, title and editor_text; visual rendering belongs to the client. No handler produces `Unsupported`, never an indefinite wait. This channel does not introduce tool approvals. Authentication uses its separate private API: raw authentication terminals returned by `wait` must not be broadcast or persisted as public events.

## CLI and later frontends

A non-TTY print stdin is UTF-8 text, preserved exactly and separated from the first command-line prompt by two newlines. Attachments join only the first submission. Additional positional prompts run in order and stop after the first failure or cancellation; text and JSON modes use the same exit outcome. RPC and authentication commands own their own stdin. A pipe EOF finishes print input but closes and cleans up an RPC connection.

The full-screen TUI and its terminal renderer remain separate frontend work. HTML export, sharing and update services are not part of this entrypoint contract; no permanently failing placeholder methods are advertised for them.
