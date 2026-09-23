# Export and explicit sharing

`eden history export HISTORY BACKUP.jsonl` remains a complete public-history backup. It does not load the original conversation plugins. The separate reading export omits private installation state and is not a restore format. Explicit `html` format requests and `.html` CLI preview destinations are rejected.

```sh
eden export HISTORY PREVIEW.jsonl
eden export HISTORY SELECTED.jsonl --selection '{"head":42,"runs":[3,4],"thinking":true,"attachments":true}'
eden share PREVIEW.jsonl --sha256 DIGEST_FROM_EXPORT --confirm
```

Open and inspect the preview before publication. Export defaults to the current branch's user/assistant messages and ordinary tool calls/results. `head` selects an ancestor path; `runs` selects turn run IDs. The boolean categories `messages`, `tools`, `thinking`, `attachments`, `full_outputs` and `extensions` control the actual artifact bytes. Thinking, inline attachments and additional complete tool outputs are opt-in. Configuration, composition paths and opaque provider/plugin state are excluded; unknown namespaced records have a generic summary. Ordinary text and tool results can contain sensitive information; this is not automatic redaction. Missing full outputs produce warnings. The source history is never changed, and damaged history must first be explicitly recovered using the history/session tools.

Reading JSONL starts with `eden-reading-v1` and `restorable:false`; it is deliberately distinct from backup transactions.

Export prints a SHA-256 identity for the saved file. Share reads those exact bytes and rejects a modified preview; later conversation or branch changes cannot change the upload. It requires `--confirm` and a `GH_TOKEN` with gist permission. The `github-share` package also accepts a `token_env` setting for another environment variable. No credential is included in the artifact. The default endpoint is GitHub's gist API; an explicit HTTPS endpoint or loopback HTTP fixture may be configured for a controlled publisher.

The official publisher creates a **secret gist**. Anyone with its URL can access it; secret is not private access control. Download the JSONL for local inspection. No Pi viewer or Eden-hosted viewer is required. Publication never retries automatically: a lost response, late cancellation or ambiguous server failure reports `PublicationUnknown`, because the gist might already exist. Check the account before explicitly retrying. Cancelling does not promise to undo a remote creation.

## Rust and RPC

Saved semantic presentation views are projected as `type: "presentation"` reading entries using the same selection. Their source record and class must survive the selected ancestry; tool-sourced rich views also require `full_outputs:true` because arbitrary nodes cannot be split safely into short versus complete output. Excluded content cannot reappear through title, fallback, node or attachment reference. Business actions are removed from exported views. Copying public history remaps saved source and attachment references. The independent `eden read HISTORY --endpoint PATH` host opens complete history without its business plugins; it does not accept reading JSONL as restorable input.

`Session::export(Selection, Format)` returns an in-memory `Artifact` from committed history, including for memory-only sessions. It does not write a preview file. `Session::publish(PublishRequest)` and `Session::update(UpdateRequest)` return owned run IDs; use the existing wait/cancel/shutdown API. The client presents the artifact for review and sets `confirmed` only after explicit user authorization.

`eden_agent::delivery::Delivery::open` loads only requested delivery roles from an explicitly trusted installation composition, independently of the old session's business packages. `export_file` reads validated public history; `invoke` accepts the same public protocol data and a cancellation token. Always await `shutdown`. `save_preview` creates a new file and returns its identity; `read_preview` rejects bytes that changed since review and files without the reading JSONL marker, including complete history backups. The official publisher applies the same marker check to in-memory artifacts.

RPC methods `export`, `share` and `update` use these same Session operations. `export` accepts `{ "selection": {}, "format": "jsonl" }` and returns the artifact plus `preview_id` without file I/O; use `share` with `{ "preview_id": "...", "confirmed": true }` to publish large prepared artifacts without sending their bytes back through the input frame limit. Up to eight previews are retained per Session; release unused ones with `preview.forget` and `{ "preview_id": "..." }`. Rust callers have matching `prepare_export`, `publish_preview`, and `forget_preview` methods. Alternatively, `share` accepts `{ "artifact": ..., "confirmed": true }`; `update` accepts the tagged UpdateRequest documented in [updates](updates.md). Publication/update acceptance is separate from the terminal event. RPC stdout remains protocol frames only.

## Native extension roles

The SDK exports `protocol::delivery::{EXPORTER, SHARE_TARGET}` and `protocol::updates::UPDATE_SOURCE`. Implement ordinary `Package::service` handlers for `ExportRequest -> Artifact`, `PublishRequest -> PublishReply`, and `UpdateRequest -> UpdateReply`. Roles are independently selected in a composition; no renderer or framework Rust type crosses the native boundary. A delivery-only composition needs no agent-loop/context/provider/tool roles. Reading artifacts intended for the official publisher need an `eden-reading-v1` header with `restorable:false`. The independent `tests/contract-authors/delivery-services` package demonstrates all three replacements and is built after freezing the installed host in acceptance tests.

## Startup maintenance

Update discovery runs in a session-owned background task and emits `update_check` events, separate from conversation history. A slow or unavailable update source does not delay session readiness. Shutdown cancels and waits for the task. Use `--no-update-check` or settings `update_check:false` to disable it. `--offline-startup` or settings `offline_startup:true` suppresses official startup maintenance networking, including update discovery. Current model catalog, subscription, credential and router registration reads local data without startup requests; explicit later model refresh, authentication, update, provider or tool operations still work. This option is not a process-wide network sandbox.

S7 acceptance uses controlled publication/update sources. A real gist upload and a first official Release are separate operations; passing the controlled suites does not claim those service results.
