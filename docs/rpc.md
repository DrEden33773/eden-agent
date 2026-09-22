# Stdio RPC v1

Run `eden --composition PATH rpc`. The connection initially opens one in-memory Session in `--cwd` (the current directory by default). `--session PATH` opens or creates a persistent session using the CLI's saved-cwd rules. Global workspace options such as `--global-dir` and `--trust-project` retain their normal meaning. Positional prompts, attachments, `--print`, and prompt-only options are rejected; stdin belongs exclusively to RPC. RPC output is always compact, uncolored JSONL, regardless of `--json` or `--color`.

The first output is `{"version":1,"type":"ready","session_id":123,"methods":[...],"limits":{"frame_bytes":1048576,"output_messages":256},"interactions":false}`. Wait for it before sending requests. `methods` lists transport operations; a selected composition can still lack an operation's plugin role. Missing roles return their normal Session fault. Session IDs are unsigned 64-bit values: clients must preserve integer precision rather than round them through JavaScript `Number`.

## Requests and replies

Each request is one UTF-8 JSON object, terminated by LF. CRLF is accepted. U+2028 and U+2029 inside strings do not split frames. A nonempty final frame without LF is parsed at EOF; EOF then closes the connection and cancels active work. Keep stdin open while waiting for a run to finish. Empty or malformed frames produce errors and leave the connection usable. An oversized frame ends the connection after an error when output remains writable. The 1 MiB limit includes the line terminator when present.

```json
{"version":1,"id":"request-1","session_id":123,"method":"prompt","params":{"text":"Explain this project"}}
```

`version`, nonempty string `id`, and string `method` are required. IDs contain at most 256 UTF-8 bytes. Every operation, including `ping` and `shutdown`, requires the current `session_id`; omitting it or supplying a stale identity returns `SessionMismatch`. `params` is an object or null and defaults to null. Unknown envelope fields are rejected. IDs must be unique among pending requests; they may be reused after completion. There are at most 64 pending requests. Full admission returns `Busy`; cancellation and synchronous interaction replies remain admitted so callers can release pending work. Duplicate pending IDs return `DuplicateId`. Asynchronous query/control responses can complete out of order; wait for a response before sending a dependent operation.

Queries and immediate controls return `{"version":1,"type":"result","id":"request-1","session_id":123,"result":...}`. Admission or request errors return the same envelope with `type:"error"` and `error:{code,source,message}`. A malformed frame whose ID cannot be recovered uses `id:null`. Long operations return `type:"accepted"` with `run_id`; this confirms admission, not successful completion. Ordered Session events use `type:"event"`, `session_id`, optional request `id`, and `event:{sequence,session_id,run_id,kind,payload}`. A run's `settled` event carries its terminal outcome and cleanup errors. A cancel result only confirms the cancellation request; wait for `settled` to observe the cleanup barrier.

## Operations

Parameters below are the `params` object. Optional fields may be omitted unless a default is explicitly required. Path arguments use the server process's normal filesystem interpretation. A new session's `cwd` determines subsequent tool and resource paths.

| Method | Parameters | Result or completion |
| --- | --- | --- |
| `ping` | `{}` | `{pong:true}` |
| `capabilities` | `{interactions:bool}` | Enables/disables semantic interactions and returns the method list; omitted `interactions` disables them |
| `state` | `{}` | Session state, active run and independent command/shell runs |
| `shutdown` | `{}` | Closes admission, cancels and awaits work, destroys native instances, then replies `{closed:true}` and exits |
| `session.new` | `{history?:path,cwd?:path}` | Replaces the current Session; omitted history means memory-only; refuses an existing destination |
| `session.open` | `{history:path,cwd?:path}` | Replaces the current Session with existing history; saved cwd is used when no override is supplied |
| `prompt` | `{text:string}` or `{content:[Block]}` | Accepted run; `content` takes precedence when present |
| `resume` | `{}` | Accepted continuation run |
| `cancel` | `{run_id:u64}` | Requests cancellation through Session |
| `history` | `{}` | Public history records |
| `tree` | `{}` | `{head,branch,records}` for the history tree |
| `queue` | `{}` | Pending entries |
| `enqueue` | `{kind:"steering" or "follow_up",text:string}` or content blocks | Accepted queue entry, including entry identity |
| `steer`, `follow_up` | `{text:string}` or `{content:[Block]}` | Entry in the corresponding queue |
| `queue.withdraw` | `{ids?:[u64]}` | Withdrawn entries; absent/null IDs select all pending entries |
| `queue.configure` | `{steering:string,follow_up:string}` | Accepted configuration run; mode validation belongs to Session |
| `control` | Serialized `CodingControlRequest` | Current/updated `CodingControlState` |
| `tools` | `{}` | Tool definitions from the selected catalog |
| `resources` | `{}` | Current resource snapshot |
| `resources.reload` | `{}` | Accepted reload run |
| `commands` | `{}` | Contributed command catalog |
| `command` | `{name:string,arguments?:JSON}` | Accepted independent command run; arguments default to `{}` |
| `metadata` | `{name:string,tags?:[string]}` | Accepted metadata run |
| `navigate` | `{target:u64,branch:string,summarize?:bool}` | Accepted history-navigation run |
| `compact` | `{instructions?:string}` | Accepted manual compaction run |
| `attachment.include` | `{record_id:u64}` | Accepted history attachment-inclusion run |
| `models` | `{}` | Model catalog |
| `model.current` | `{}` | Current model selection or null |
| `model.select` | Serialized `ModelSelection` | Accepted selection run |
| `model.catalog` | Serialized `CatalogRequest` | Accepted catalog operation |
| `router.list` | `{}` | Observed managed model state |
| `router.request` | Serialized `ManagerRequest` | Accepted router operation with ordinary model-management events |
| `auth.request` | Serialized `AuthRequest` | Accepted private operation; see authentication below |
| `auth.status` | `{operation_id:string}` | Private operation status, only in the correlated response |
| `auth.input` | `{operation_id:string,input:string}` | Private input delivery while authentication wait is active |
| `interaction.respond` | `{interaction_id:u64,value:JSON}` | `{delivered:true}` or an expired/invalid interaction error |
| `shell` | `{command:string,shell:"bash" or "powershell",exclude_from_context?:bool}` | Accepted independent user-shell run with streamed output |
| `shell.cancel` | `{run_id:u64}` | Cancels only the selected shell; wait for its `settled` event |
| `session.copy` | `{source:path,destination:path,kind:string,target?:u64,cwd?:path,public_only?:bool,apply?:bool}` | Copy plan by default; `apply:true` applies the freshly computed plan and returns `{path}` |

`session.copy` kinds are `fork`, `clone`, `import`, `upgrade`, `recover`, and `migrate`. Only fork accepts `target`; public-only and relocation restrictions are enforced by the shared copy implementation. Source bytes remain unchanged and existing destinations are refused. Copy does not switch the current Session; use `session.open` afterward. The Rust definitions of [coding requests](../crates/eden-protocol/src/coding.rs), [model/auth/router requests](../crates/eden-protocol/src/models.rs), and [copy options](../crates/eden-agent/src/management.rs) are the schemas for the serialized request families above. Their serde tags and field names are used directly. For example, `control` accepts `{"operation":"set_auto_retry","enabled":false}` or `{"operation":"stop_retry"}`; `model.catalog` accepts `{"action":"refresh"}`; `auth.request` accepts `{"action":"login","provider":"openai-codex","method":"browser"}`; `router.request` accepts `{"action":"load","model":"MODEL","unload_others":false}`. Text blocks have `{"type":"text","text":"..."}`; image/file blocks carry base64 data and media type.

Session replacement waits for the old Session's shutdown barrier before opening the new Session. Pending asynchronous queries/private authentication replies cause `Busy`; active runs are cancelled by shutdown. A successful replacement replies with the new `session_id` and then emits a new `ready`. Subsequent requests must use that identity. Missing open histories, existing new histories, and invalid cwd directories are rejected before closing the old Session. If native opening fails after the old Session has closed, the connection reports the request error and ends. A replacement cannot leave the old handle usable.

Commands use the selected native command catalog and shared Session admission rules; they are not TUI slash commands. Independent commands and user shell have their own run IDs and can be cancelled without cancelling the model. The default shell implementation streams `user_shell_output` byte chunks, applies the user-shell hook, waits for process-tree cleanup, and records its result after any earlier model run settles. `exclude_from_context` changes later projection, not history visibility. HTML export and terminal component rendering are outside this protocol version's implemented methods.

## Authentication and interactions

Authentication replies can contain login URLs, user codes, pasted redirects, and private transient data. `auth.request` returns an accepted run and later a correlated `private_result` whose `result` is the full terminal value from `Session::wait`. Ordinary public Session events retain the Session authentication redaction policy; the transport never converts the private terminal result into an event. `auth.status` and `auth.input` also reply only to their request ID. Request bodies are not logged or persisted by the transport. Clients must treat these responses as private and keep them out of shared logs.

Semantic interactions are disabled initially. Send `capabilities` with `{interactions:true}` before invoking an extension that needs a host dialog. `interaction_requested` events contain `{interaction_id,kind,title,options,initial}`; respond using `interaction.respond`. `interaction_finished` marks completion, and `interaction_notification` carries `{kind,value}` for notification/status/widget/title/editor-text intents. These are semantic data, not terminal components. Without capability support, requests fail deterministically through the shared interaction service. Session timeout, cancellation, duplicate/expired reply checks, and shutdown apply to pending interactions. Input and cancel requests remain readable while a model, command, shell, or private authentication wait is running.

## Backpressure and shutdown

One bounded writer serializes all replies and events. The output queue holds at most 256 messages; overflow ends admission and shuts down the Session rather than dropping selected events. Each actual stdout write/flush has a five-second deadline. A broken pipe or write timeout triggers the same shutdown barrier. Event observation uses the Session's bounded cursor window; a stale cursor produces `Lagged` and closes the connection. Successful output writes mean the OS accepted the bytes, not that the client application has processed them.

EOF, explicit shutdown, and signals cancel active work and await Session/native cleanup before the process exits. Normal EOF and explicit shutdown exit 0; protocol/transport or cleanup failures exit 1. SIGINT exits 130; on Unix SIGTERM exits 143 and SIGHUP exits 129, after cleanup. Final responses can only be delivered while output remains writable. Dedicated OS threads own blocking stdin/stdout so an open input pipe or blocked stdout cannot trap Tokio runtime shutdown.

## Focused checks

`cargo test --locked -p eden-cli` runs parser/framing tests and a blocked-writer test with an injected short deadline. Process tests in [rpc.rs](../crates/eden-cli/tests/rpc.rs) are explicitly ignored unless invoked with a controlled native fixture. Build the controlled standard and coding-tools libraries, prepare a composition selecting the standard loop/context/provider/tool plus `eden.user-shell.v1`, and run `EDEN_RPC_TEST_COMPOSITION=/absolute/path/to/composition.json cargo test --locked -p eden-cli --test rpc -- --ignored`. The fixture must match the installed host/SDK pairing. These tests use isolated cwd/global directories and cover frame recovery, ordering, stale session IDs, replacement, EOF, oversize input, broken stdout, shell cancellation, and process cleanup. Unix signal/process tests require an environment that delivers signals between test processes; a signal-isolating sandbox cannot provide that evidence. External plugin interaction and authentication scenarios also require the corresponding controlled provider/author fixtures in the installation verifier.

See [export, explicit sharing and startup maintenance](delivery.md) for reading snapshots and delivery services, and [updates](updates.md) for complete-installation transactions.
