# Coding sessions

The default installation selects separately replaceable native roles from `coding` (loop, context and queue), `coding-tools` (read/write/edit/bash), `model-access` (OpenAI Responses), and `local-history` (public JSONL). All calls use the SDK and versioned serialized role payloads from `eden_protocol::coding`. The original controlled `standard` package is a native lifecycle test fixture.

## Running a task

Configure an exact model with `OPENAI_MODEL` and supply a bearer credential in `OPENAI_API_KEY`. The provider also accepts `model`, a full `endpoint`, and `api_key_env` in its package `config` in `composition.json`. An explicit endpoint supports compatible services and controlled protocol verification. See [model access](../plugins/model-access/README.md) for request and authentication behavior.

```sh
eden --cwd /path/to/project --session /path/to/task.jsonl --print 'Inspect the failing test, fix its cause and run the test.'
eden --cwd /path/to/project --resume /path/to/task.jsonl --json 'Check the previous change and describe the result.'
eden --history /path/to/task.jsonl
```

The CLI creates a new JSONL file under the selected cwd's `.eden/sessions` directory when no session path is supplied. It prints the path to stderr. `--session` and `--resume` both open or create an explicit history path; its parent directory must already exist. `--no-session` uses memory and creates no history file. Existing histories use their recorded cwd when `--cwd` is omitted. An explicit different cwd is rejected; use a source-preserving migration with `--cwd` to relocate. Selected role bindings stay fixed; compatible package versions and unused package inventories do not by themselves require migration. There is no automatic history selection or background execution on reopen.

`--attach PATH` embeds a UTF-8 text file; `--image PATH` embeds PNG/JPEG/GIF/WebP data; `--file PATH` embeds PDF data. Paths resolve against `--cwd`. The public blocks contain the bytes encoded as base64, so resume does not depend on the original attachment remaining at a temporary path. The selected model must support the input modalities. Credentials are not included in history records.

`--json` emits ordered live events to stdout. `accepted` identifies the admitted run; `committed` identifies confirmed records; model delta events are transient. `settled` includes the fixed outcome and any cleanup/persistence errors. A successfully accepted or partially streamed run can still fail. Failed operations exit nonzero; cancelled runs exit 130; stdout failures trigger session shutdown. Ctrl-C requests cancellation and waits for cleanup.

## Command line

Every command and option is declared once in a clap command tree, so the tree is the only place an accepted spelling exists. Help is layered and derived from it: `eden --help` lists the command families, `eden session --help` lists the actions of a family, and `eden session fork --help` describes one action's arguments and options. `-h`, `--help`, `-V` and `--version` write to stdout and exit 0.

A usage error — an unknown option, a misspelled action, a value a flag does not accept, or two options that contradict each other — is reported before anything runs, exits **2**, and names the closest accepted spelling when there is one. A run that started and failed exits **1**. `eden history inspect` on a damaged tail still exits **2** after printing the records it validated, and a cancelled run exits **130**.

stdout always carries machine-readable results. Human-readable text, including resource diagnostics, goes to stderr, so `--json` output stays a clean event stream while the reason a project's resources were ignored remains visible. `--color auto|always|never` chooses whether human-readable output is colored: `auto` colors only a terminal, and `never` removes color even there.

`-q`/`--quiet` suppresses status lines, warnings and notes; it never suppresses an error, because a failure is a result rather than narration. `-v`/`--verbose` is accepted and repeatable, and reserves the channel for detail; nothing emits detail yet. `--json` and `--quiet` answer different questions. The first keeps stdout machine-readable and touches nothing else; the second empties the human channel, so a run that succeeds under `--json --quiet` writes nothing at all to stderr while the same diagnostics stay readable as events. A run that fails still prints its error.

## Explicit environment files and DeepSeek

The CLI accepts `--env-file PATH`. Relative paths are resolved against the invoking process cwd, independently of `--cwd`. Only that file is parsed; neither the project nor parent directories are searched. Parsing treats the file as dotenv data and never executes shell commands. The entire file is validated before variables are installed, and this happens before the async runtime or plugins start. Parse diagnostics omit file contents.

Copy [`.env.example`](../.env.example) to a local `.env`, fill in `DEEPSEEK_API_KEY`, and select it explicitly. `.env` and `.env.*` are ignored by Git; the example remains tracked. Existing process variables win over the file, including explicitly empty variables; explicit model-access package settings retain priority over environment settings. Repeated assignments within the file use the last value.

```sh
eden --env-file /absolute/path/to/.env --cwd /path/to/project --session /path/to/task.jsonl --print 'Inspect and fix the failing tests.'
```

The example selects the DeepSeek Responses profile and `deepseek-flash`. Its ordinary coding output allowance is the documented maximum, 384K (393216 tokens), and effort is `high`. Summary requests use the separate allowances documented below. This is an output allowance, not an instruction to generate that many tokens. The coding loop has no request-count or monetary cap. DeepSeek-specific reasoning and image behavior, supported parameters and file limitations are described in [model access](../plugins/model-access/README.md).

## Tools and side effects

The context publishes the schemas for `read`, `write`, `edit` and `bash`. File paths resolve against the session cwd; absolute paths are accepted. These trusted native tools run with the caller's filesystem and process authority. `read` supports a 1-based line offset and line limit. `edit` requires exactly one occurrence of `old_text` and preserves the original file when there are zero or multiple matches. `write` creates parent directories. File text is UTF-8.

Tool output is bounded to 65,536 bytes, on a UTF-8 boundary, with an explicit `truncated` flag. Shell output includes an exit code, and nonzero exit or file errors produce structured tool failures that are visible to subsequent model requests. A completed command owns its own terminal status: cleanup clears descendants that outlived the shell and never signals the shell itself, so finishing a command cannot turn its real exit code into a signal death. A cancellation is reported through `ToolResult.error`, and its absent exit code is not a signal death: an exit code is missing either because the cancellation stopped the shell or because the command failed before a shell started. Bash must be available; `coding-tools` config can set its `bash` executable path. A missing shell gives a concrete failure. POSIX process groups and Windows Job Objects own shell descendants through the cancellation barrier; native plugins are trusted code, not a sandbox.

The loop commits model output and all tool intentions before beginning a tool round. Each result is committed before the next model request. A persistence failure stops execution; an external change may already have occurred when recording its result fails. No storage error silently selects memory mode. Provider failures do not replay the run or retry tools.

## Public history and independent reading

The selected SessionStore is the only history writer. Version 2 JSONL uses one transaction envelope per physical line: `{"schema_version":2,"transaction":[...]}`. A batch contains complete public records; its newline commits the whole batch. Each record has a stable per-session `sequence` node identity, `parent_id`, `branch`, kind and payload. The store serializes append batches and calls `sync_all` before issuing a receipt. A response and its queue-consumption records share a transaction, and all tool intentions are committed before tools execute.

The store holds an exclusive OS lock on `<history>.lock` until close. A second writer is rejected while the data file remains independently readable on Windows and POSIX. Lock files remain after close to preserve a common arbitration object. The independent reader validates schema, sequence, identity, parent links and complete transactions. A damaged or unterminated transaction exposes only its previously validated prefix, with a diagnostic; it never skips middle corruption or repairs bytes. Filesystem durability follows the operating system's sync guarantees; new independent copies are synced before exclusive atomic publication.

```sh
eden history inspect /path/to/task.jsonl
eden history export /path/to/task.jsonl /path/to/export.jsonl
eden session info /path/to/task.jsonl
eden session tree /path/to/task.jsonl
```

These commands do not load native business libraries or require the original cwd. `inspect` prints expanded records and exits 2 when the source has a damaged tail. Export preserves public JSONL transaction framing and refuses an existing destination. Rust consumers use `eden_kernel::history::inspect` for a prefix plus diagnostic or `read` for strict validation; `eden_protocol::history::active_path` selects the current ancestry.

## Trees, copies and migration

```sh
eden session branch task.jsonl --at 12 --branch alternate
eden session branch task.jsonl --at 24 --branch main --summarize
eden session metadata task.jsonl --name 'Parser repair' --tag parser
eden session fork task.jsonl fork.jsonl --at 12
eden session fork task.jsonl fork.jsonl --at 12 --apply
eden session clone task.jsonl clone.jsonl --apply
eden session migrate task.jsonl moved.jsonl --cwd /new/project --apply
```

Tree navigation preserves prior nodes and changes only the active path. A branch name associates queued inputs with that branch. `--summarize` explicitly asks the model to carry work from the departed path into the selected path; failure restores the previous selection. Navigation, copies and summaries never roll back project files. History-node operations are serialized with run admission.

Copy commands print a preservation/loss preview by default; `--apply` explicitly creates the independent destination. Fork copies the selected ancestry, clone/import preserve the tree and active selection, and every copy gets a fresh session identity plus source provenance. Pending queue work stays in the original session; consumed queue messages remain conversation and delivery preferences remain settings. Attachments are embedded in the copied records. `Session::plan_copy` returns a preview tied to exact source bytes; `apply_copy` rejects a source changed after preview and never overwrites the destination.

Version 1 linear history remains independently readable. `session upgrade SOURCE DEST` previews an explicit conversion to version 2; add `--apply` to create the new session before continuing. `session recover SOURCE DEST` explicitly copies only a validated prefix from damaged history, preserving the original including its bad tail. `session import SOURCE DEST` handles eden's own public format, not Pi session files.

The current branch's latest `extension_state` per namespace declares its state version and whether continued execution requires interpretation. Missing display-only extensions preserve public payload/summary without preventing a request. A missing or incompatible required interpreter fails before model access. The `eden.record-interpreter.v1` and `eden.state-migrator.v1` roles supply interpretation and explicit conversion. Migration previews converted state and losses, validates the result with the target interpreter, and replaces states at their corresponding copied tree nodes. A converter's applied result must match its preview. `session migrate ... --public-only` explicitly omits plugin/private Provider state; it does not claim complete state restoration. Restore an original combination or select migration explicitly when required state is unavailable.

## Context and recovery

```sh
eden session compact task.jsonl --instructions 'Preserve parser constraints and observed test results'
eden session include-attachment task.jsonl --at 7
eden --session task.jsonl --continue --json
```

The independently replaceable ContextStrategy owns the raw-record projection and summary policy. The default preserves original records, approximately 20,000 recent tokens, complete tool-call/result groups and necessary recent Provider state. Compaction emits a structured summary with goal, constraints, progress, decisions and next steps; repeated compaction incorporates the previous summary. Cumulative file paths and unresolved historical tool effects are retained separately from the generated text. Summaries are model-generated, not a lossless record replacement. Old binary attachments stay in the raw history and can be explicitly reintroduced by record ID; the summary request receives textual attachment references.

The coding package accepts `compaction: {enabled, reserve_tokens, keep_recent_tokens}` and `retry: {max_retries, base_delay_ms}` in its explicit composition config; the defaults are true/16384/20000 and 3/2000. Short or already compacted transcripts without older material return `compaction_skipped` and retain recent messages. Automatic compaction is enabled when model limits are known and estimated context exceeds `context_window - 16384`; an unknown context window disables threshold detection, while manual compaction remains available. Large single turns may split at complete tool-round boundaries. History summaries allow up to 13,107 output tokens, or 8,192 for a split-turn prefix, bounded by the selected model's maximum. Failed, cancelled or truncated summaries do not become valid compaction checkpoints.

The Responses provider sends once by default. The loop handles typed transient failures with at most three additional attempts and cancellable 2/4/8-second delays; summary requests use the same recovery policy. Authentication, quota/billing, invalid input and persistence failures are terminal. A context-overflow or recoverable truncated response gets one compact-and-retry attempt; a completed response reporting usage above the window is compacted without replaying it. Failure records remain in history but do not become successful assistant context. Prior tool results are reused; no retry replays the whole run or its executed tools.

A historical tool intention without a result becomes an explicit unknown-outcome note, never an automatic tool execution. Opening a session restores history and pending-input state only. `--continue`, `session continue`, or `Session::resume()` explicitly starts execution without adding another user message.

## Queues and Rust callers

```sh
eden session enqueue task.jsonl 'Check edge cases next' --kind follow_up
eden session queue task.jsonl
eden session queue-mode task.jsonl --steering all --follow-up one
```

Queue entries have stable identities and a branch binding. Steering is delivered at a complete tool-round boundary before the next model request; follow-up is delivered after current work would finish. Both default to `one` and support `all`. Acceptance, delivery and consumption are distinct records. Delivery associates with a logical model request, and consumption commits atomically with that complete response and its tool intentions. Failure/cancellation before response commit returns the original entry; after consumption it does not requeue or repeat tool effects. Reopening does not automatically consume pending entries.

Rust callers use `Session::open_with`, `open_saved`, `submit_blocks`, `resume`, `navigate`, `compact`, `plan_copy`, `apply_copy`, `enqueue`, `queued`, `wait`, `history` and `shutdown`. `Session::open` is memory-only. Memory sessions create no history files on normal or failed exit, and persistence errors never silently select memory mode. Await `shutdown` to close admitted work and release all native resources.

## Verification

```sh
cargo test --workspace --locked
python3 scripts/verify.py
python3 scripts/verify-coding.py
python3 scripts/verify-sessions.py
python3 scripts/verify-context.py
```

The coding verifier uses a controlled Responses server while executing actual file changes and shell checks in temporary projects. It reopens history and tests independent Provider, tool, context and store replacements against the already built host. The native CI matrix runs on Linux, Windows and macOS and publishes `coding-verification.json`. Credentialed model task results are recorded separately from controlled protocol results.
