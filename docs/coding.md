# Coding sessions

The default installation selects separately replaceable native roles from `coding` (loop, context and queue), `coding-tools` (read/write/edit/bash), `model-access` (OpenAI Responses), and `local-history` (public JSONL). All calls use the SDK and versioned serialized role payloads from `eden_protocol::coding`. The original controlled `standard` package is a native lifecycle test fixture.

## Running a task

Configure an exact model with `OPENAI_MODEL` and supply a bearer credential in `OPENAI_API_KEY`. The provider also accepts `model`, a full `endpoint`, and `api_key_env` in its package `config` in `composition.json`. An explicit endpoint supports compatible services and controlled protocol verification. See [model access](../plugins/model-access/README.md) for request and authentication behavior.

```sh
eden --cwd /path/to/project --session /path/to/task.jsonl --print 'Inspect the failing test, fix its cause and run the test.'
eden --cwd /path/to/project --resume /path/to/task.jsonl --json 'Check the previous change and describe the result.'
eden --history /path/to/task.jsonl
```

The CLI creates a new JSONL file under the selected cwd's `.eden/sessions` directory when no session path is supplied. It prints the path to stderr. `--session` and `--resume` both open or create an explicit history path; its parent directory must already exist. `--no-session` uses memory and creates no history file. Existing histories require the same canonical cwd and package/role binding. There is no automatic history selection or background execution on reopen.

`--attach PATH` embeds a UTF-8 text file; `--image PATH` embeds PNG/JPEG/GIF/WebP data; `--file PATH` embeds PDF data. Paths resolve against `--cwd`. The public blocks contain the bytes encoded as base64, so resume does not depend on the original attachment remaining at a temporary path. The selected model must support the input modalities. Credentials are not included in history records.

`--json` emits ordered live events to stdout. `accepted` identifies the admitted run; `committed` identifies confirmed records; model delta events are transient. `settled` includes the fixed outcome and any cleanup/persistence errors. A successfully accepted or partially streamed run can still fail. Failed operations exit nonzero; cancelled runs exit 130; stdout failures trigger session shutdown. Ctrl-C requests cancellation and waits for cleanup.

## Explicit environment files and DeepSeek

The CLI accepts `--env-file PATH`. Relative paths are resolved against the invoking process cwd, independently of `--cwd`. Only that file is parsed; neither the project nor parent directories are searched. Parsing treats the file as dotenv data and never executes shell commands. The entire file is validated before variables are installed, and this happens before the async runtime or plugins start. Parse diagnostics omit file contents.

Copy [`.env.example`](../.env.example) to a local `.env`, fill in `DEEPSEEK_API_KEY`, and select it explicitly. `.env` and `.env.*` are ignored by Git; the example remains tracked. Existing process variables win over the file, including explicitly empty variables; explicit model-access package settings retain priority over environment settings. Repeated assignments within the file use the last value.

```sh
eden --env-file /absolute/path/to/.env --cwd /path/to/project --session /path/to/task.jsonl --print 'Inspect and fix the failing tests.'
```

The example selects the DeepSeek Responses profile and `deepseek-flash`. Its output allowance is the documented maximum, 384K (393216 tokens), and effort is `high`. This is an output allowance, not an instruction to generate that many tokens. The coding loop has no request-count or monetary cap. DeepSeek-specific reasoning and image behavior, supported parameters and file limitations are described in [model access](../plugins/model-access/README.md).

## Tools and side effects

The context publishes the schemas for `read`, `write`, `edit` and `bash`. File paths resolve against the session cwd; absolute paths are accepted. These trusted native tools run with the caller's filesystem and process authority. `read` supports a 1-based line offset and line limit. `edit` requires exactly one occurrence of `old_text` and preserves the original file when there are zero or multiple matches. `write` creates parent directories. File text is UTF-8.

Tool output is bounded to 65,536 bytes, on a UTF-8 boundary, with an explicit `truncated` flag. Shell output includes an exit code, and nonzero exit or file errors produce structured tool failures that are visible to subsequent model requests. Bash must be available; `coding-tools` config can set its `bash` executable path. A missing shell gives a concrete failure. POSIX process groups and Windows Job Objects own shell descendants through the cancellation barrier; native plugins are trusted code, not a sandbox.

The loop commits model output and all tool intentions before beginning a tool round. Each result is committed before the next model request. A persistence failure stops execution; an external change may already have occurred when recording its result fails. No storage error silently selects memory mode. Provider failures do not replay the run or retry tools.

## Persistence and queues

The selected SessionStore is the only history writer. A JSONL line is one public record and one commit boundary. The default store serializes appends, writes a newline and calls `sync_all` before issuing a receipt. It holds an exclusive OS lock on a sibling `<history>.lock` file until close; another writer is rejected while the data file remains independently readable on Windows and POSIX. The lock file is retained after close so all writers continue to arbitrate on the same filesystem object. `eden_kernel::history::read` validates public schema, identity and contiguous sequence without loading a storage plugin. Interrupted or malformed tails are diagnosed and preserved. Filesystem and hardware durability still follow the operating system's `sync_all` guarantees.

Resume projects committed messages and correlated tool results. A historical intention without a result becomes an explicit unknown-outcome note; it is never automatically executed. Original records remain available. Full branch operations, migration and compression are future additions.

Rust callers use `Session::open_with` with `SessionOptions { cwd, history }`, `submit_blocks`, `wait`, `history` and `shutdown`. `Session::open` creates a memory session. `enqueue("steering", blocks)` and `enqueue("follow_up", blocks)` return durable queue identities. Steering is delivered one entry at a tool-round boundary; follow-up is delivered one entry after an answer. Delivery is recorded separately from acceptance. Undelivered entries survive cancellation and persist across reopen; `queued()` exposes them. Submitting a new run remains explicit.

## Verification

```sh
cargo test --workspace --locked
python3 scripts/verify.py
python3 scripts/verify-coding.py
```

The coding verifier uses a controlled Responses server while executing actual file changes and shell checks in temporary projects. It reopens history and tests independent Provider, tool, context and store replacements against the already built host. The native CI matrix runs on Linux, Windows and macOS and publishes `coding-verification.json`. Credentialed model task results are recorded separately from controlled protocol results.
