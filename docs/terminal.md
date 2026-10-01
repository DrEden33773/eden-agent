# Interactive terminal

The installed `eden` entry calls the Grok-derived terminal library in the same process. Build it with [the frontend build guide](../frontend/README.md). New sessions start as drafts; model selection, browsing and unsubmitted input do not create history. The first accepted prompt or shell command saves identity and intent before execution. Explicitly naming a draft also saves it. `/resume` and `/sessions` open the same history picker.

All creation, owner validation, attachment and restoration use `eden-session-workspace`. The service validates the persistent ID, canonical history locator and host instance before attachment. Each load receives a separate local view identity. Loading failure returns to the previous view with its unsent draft; notifications for a previous view cannot change the new view. Binding incompatibility opens a plugin-free reader and reports why execution is unavailable. Migration writes an explicitly reviewed new copy and preserves its source.

Exiting the terminal detaches from execution hosts; accepted work continues there. Explicit stop waits for host cleanup and writer release. A reader created by this frontend is closed during frontend cleanup, while an explicitly borrowed reader remains alive. No-target startup opens an unsaved draft. Use `--session PATH` for an explicit resume, `eden tui --read PATH` for immutable reading, or `eden tui --endpoint PATH` to attach a selected host. `EDEN_LEGACY_SESSION_DIRS` may list additional directories using the platform path separator; discovery filters them by canonical project cwd and never moves or rewrites them.

`eden tui --frontend native` selects the SDK terminal described below. An explicit `--editor` also selects it so independent UI authors continue to exercise their installed plugin. Its drafts and semantic presentation contract remain available alongside the default frontend.

## SDK terminal entry

Run the installed `bin/eden` with `tui --frontend native` in a terminal to start a new task. The terminal attaches to a separate local Session host, so closing the frontend leaves accepted work running. The exit message prints the exact endpoint to reconnect. `--print`, `--json`, piped input, explicit prompt runs and RPC keep their existing output and lifecycle.

```sh
eden tui --frontend native
eden tui --frontend native --endpoint /path/to/host.json
eden --session /path/to/history.jsonl tui --frontend native
eden tui --frontend native --read /path/to/history.jsonl
```

An endpoint selects an already live host. A history path restores a stopped session without running another turn. Read-only history loads committed public records without loading the saved business plugins. `--composition`, workspace trust, resource discovery, tool selection, model and thinking startup settings are passed to a newly created or restored host. They are not silently applied when attaching to someone else's host. Use `--tools=read` when a tool name also names an Eden command.

## Daily interaction

| Action | Default input |
| --- | --- |
| Submit / newline | Enter / Shift+Enter or Ctrl+J |
| Complete commands, resources or files | `/`, `@`, Tab |
| Steering / follow-up | Alt+S / Alt+F |
| Queue review and withdraw into the existing draft | `/queue` |
| Queue delivery policy | `/queue-mode one` or `/queue-mode all` |
| User shell included / excluded from model context | `!command` / `!!command` |
| Cancel a separately owned shell | `/shells` |
| Search loaded branch content | Ctrl+F |
| Read / follow newest | PgUp, PgDn / Ctrl+End |
| Inspector / turn navigation / change focus | F3 / F4 / F6 |
| Expand tool or reasoning | Ctrl+O |
| Interrupt the active run; when idle, copy selection / clear unselected composer | Ctrl+C |
| Copy last answer / paste clipboard | `/copy` / Ctrl+V |
| External editor | Ctrl+G |
| Undo / redo / yank previous kill | Ctrl+Z / Alt+Z / Alt+Y |
| Suspend and resume on Unix | Ctrl+Alt+Z, then shell `fg` |
| Help / commands / display settings | F1 / F2 / F10 |
| Close local view, then request foreground cancellation | Esc |
| Detach / stop the host | Empty Ctrl+D or `/quit` / `/stop-host` |

Cancellation acceptance and cleanup completion are distinct. A busy ordinary send retains the draft. A lost submission response retains its original request identity; Ctrl+R checks that request rather than submitting another turn. Bracketed paste always edits the composer first, including pasted slash commands and shell commands.

Files selected through `@` or `/attach PATH` are captured as content snapshots. `/attachments` reviews and removes them. Queued content and saved drafts retain their bytes even if the source file changes. Image adaptation and inline terminal image rendering are separate model-input capabilities; this frontend transports supported encoded image attachments through the shared host.

System clipboard operations run outside the input/render loop. A successful platform backend reports success; OSC52 only reports that a request was sent to the terminal. SSH clipboard reads require a local system backend and are not inferred from an OSC52 write. Complete retained tool output is available through `/artifacts` for the selected tool; missing artifacts report a read failure.

## Local drafts and display settings

Ordinary composer text and attachment snapshots are saved locally under the user state directory (`EDEN_TUI_STATE_DIR` overrides it). Default identities distinguish terminal windows where the terminal exposes a stable identity. `eden tui --frontend NAME` selects an explicit identity; use different names for concurrently edited frontends. `/recover` restores only a draft associated with the explicitly selected Session. Drafts have no silent expiry. Configuration forms and secret values are excluded from ordinary draft files.

`ui.json` in that directory stores local display settings. F10 changes them immediately and saves that scope. The file is reloaded while running; invalid settings retain the previous effective configuration. Disabling the footer removes its rows. Terminal-default backgrounds and `NO_COLOR` are supported.

The `colors` object accepts 24-bit RGB integers for `foreground`, `muted`, `accent`, `success`, `error`, `border`, `heading`, `key`, `code`, `link`, `quote`, `number`, `type`, `function`, `dim` and `warning`. The `bindings` object maps a context and key to an action, for example `"composer.ctrl+enter": "send"`. Contexts are `composer`, `transcript`, `inspector` and `modal`; key modifier order is `ctrl+alt+shift+`. Available actions are `send`, `cancel`, `copy`, `clear`, `external_editor`, `quit`, `search`, `inspector`, `navigator`, `settings`, `steering`, `follow_up`, `paste`, `undo`, `redo` and `close`.

## Presentation and configuration

Standard contributed text, code, Diff, tables, attachments and fallback content appear with their plugin owner. `/live` opens the available form or action. `/config INSTANCE` asks the shared host to open an instance's configuration view. Typed controls use the shared field schema; JSON remains the fallback for non-secret structures. Ctrl+V validates, Ctrl+P previews, Ctrl+S applies, Ctrl+K cancels an application, Ctrl+F refreshes, Ctrl+D discards the local form draft, and Ctrl+B selects inheritance. A changed binding preserves the draft for review instead of overwriting current configuration.

Secret edits use the private-input channel. The frontend clears material after sending and retains only the public retry envelope. A retry that the host never accepted requires re-entry; an accepted request can recover without resending the secret.

## Native UI roles

Native UI code runs in the terminal process. `eden-ui-sdk` defines C-compatible versioned tables; it does not pass Rust framework objects across a dynamic-library boundary. The default editor is installed in `ui/`. Explicit trusted replacements are selected with `EDEN_TUI_EDITOR`, `EDEN_TUI_RENDERER`, `EDEN_TUI_THEME` and `EDEN_TUI_FRONTEND`. `eden tui --editor PATH` also selects an editor. The renderer receives structured messages and display settings, the theme resolves semantic tokens, and a replacement frontend receives the endpoint and owns its own terminal lifecycle. Native libraries have the same authority as the frontend process.

The SDK provides `export_ui!` and one safe trait per role, so an author implements a Rust type and writes no `unsafe`, no entry point and no table; the macro also derives each table's version and size from the SDK types, so a hand-computed `table_size` cannot drift. A hand-written `#[repr(C)]` table remains public and supported. The [native UI plugin guide](ui-plugins.md) documents the traits, the per-role failure values, the borrowing rules and complete examples.

See the [SDK](../crates/eden-ui-sdk/src/lib.rs) and the independent authors [tui-editor](../tests/contract-authors/tui-editor/README.md) (hand-written tables) and [ui-macro](../tests/contract-authors/ui-macro/README.md) (macro-exported roles). `python3 scripts/verify-tui.py` checks the installed boundary for both; POSIX runs use a real PTY. Core/editor/renderer tests also run natively on the supported CI platforms.

For targeted frontend tests, first run `cargo build -p eden-terminal-editor --locked`, then `cargo test -p eden-tui --locked`. The tests load the actual dynamic library; a Cargo test build alone does not publish it in the normal library output directory. CI builds this prerequisite explicitly.

## Rich content

The terminal renders Markdown, highlighted code, tables and responsive split/unified Diff. Supported Mermaid flowcharts and sequence diagrams are laid out as terminal text. Common LaTeX symbols, fractions, roots and scripts are rendered natively in inline, display and fenced formulas. Unsupported or incomplete syntax retains readable source. Displayed content cannot emit terminal control sequences.

## Inspect and edit model context

Open `/context` or press F7 to inspect the shared model input. The default view is an independent effective draft; Tab cycles through original input, preview diff and the last captured actual model request. The original transcript remains intact, with a short marker for each committed edit. The view includes system messages, tool declarations and structured tool calls/results. Edit records show pending or used status from actual request records; applying while a model is running affects a subsequent safe request boundary.

Use arrows or click to select an entry, Enter or `e` to edit its first text block, `v` to cycle its message role, `n` to insert a message, `x` to exclude an entry, and `[` / `]` to reorder it. Other blocks in a message are retained by text editing. Use `j` for the selected structured item or tools and Shift+J for the complete document JSON; inserted entries need unique IDs. Ctrl+S saves an editor field into the local draft and Escape cancels that field. Tool calls and results must remain complete and correctly ordered; preview and the host reject invalid structures before any change is committed.

Press `s` to choose the current branch (default) or the next logical request, including its retries. Press `p` to validate and preview, then `a` to apply the reviewed draft. Escape closes the view while retaining its draft in the current frontend. After a conflict, `b` explicitly rebases the draft onto a fresh inspection, preserving newly appended input; review and preview again before applying. `r` requests a fresh copy and asks for `y` before discarding the draft. Transport failures retain the original transaction: `t` retries the same request ID without creating another edit. Pending operations block draft mutations. These context drafts are separate from the composer and are retained in memory, not restored after frontend exit.

Shift+R in the context view, or `/context-rebuild`, opens a rebuild form. Edit the proposed fresh branch name, use Space to include or exclude saved branch-scope edits, and choose Start or press Ctrl+S. Rebuild uses original execution records plus the selected persisted edits; it does not apply the unsaved context draft, call a model or replay tools. `/compact` or `c` opens a separate instructions field and Start action for compaction, which calls the configured model policy. Both operations require an idle writable session. Admission shows the run ID; only the terminal result reports completion. `t` recovers the original admission or waits for the same run after a transport failure.

A completed operation refreshes into the Operation result view while retaining the existing context draft and its original revision. Explicitly reload or rebase to continue editing from the new state. Failed operations retain the context draft and show their failure separately from admission. Tab or Shift+P opens the policy inspector, which displays the configured order, boundary, enabled state and settings without running policies. Press `g` there, or use `/config coding`, to change the existing shared configuration; the inspector does not create a separate policy store.

Shift+B in the context view shows the current model, effective compaction budgets with each value's global or exact-model source, and actual image limits. Unknown values remain unknown. Press `g` to open `/config coding` for shared model overrides. Shift+I opens images: arrows select an image and `o` switches between its retained original and active sent version. The view reads committed image-version records, including omission state and preparation metadata. `1` stages Preserve, `2` stages Omit and `3` stages Re-adapt; `p` reviews the choices and `a` submits them together against the captured revision. A conflict or unsupported model retains those choices; no image is silently omitted. Reload explicitly discards choices; rebase updates their captured revision for another review. Image submissions leave the separate document draft intact.

Submitted original images appear as cached raster thumbnails in the main conversation, with their rows included in scrolling and resize layout. The context image inspector separately offers the retained original and active sent version. Previews decode on a background worker and cache their pixels. Direct Kitty terminals use the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) with locally encoded PNG data, suppressed replies and a placement cleared on close, resize or terminal handoff. Other terminals, including multiplexers, use a half-block raster preview; no-color mode uses monochrome characters. Animated images show a still preview. Preview decoding errors leave the retained image and its editing choices available; preview size does not change the bytes sent to the model.

## Fixed session references

Complete `@session` in the composer, or use `/session`, to choose a saved source session and branch. Type in the session catalog to filter names, paths and branches; Backspace edits the query and Ctrl+U clears it. A matching selected source remains selected while the query changes. This reads a fixed preview without switching the active session. Arrow keys select rows; Space includes or excludes an exchange or an individual image. Images are excluded by default and require their parent exchange to remain selected. Press `s` to inspect captured source instructions; a missing snapshot stays unknown. The comparison describes the inspected target projection and is checked again against final instructions when sending. Tool calls and results are quoted source material, not executable work.

The preview shows a token estimate and estimated remaining capacity using the current model and resolved reserve. Unknown limits remain unknown; an over-budget or unsupported-image warning does not silently truncate, summarize or discard the selection. Press `i` to insert the owned snapshot, or Escape to cancel while preserving the composer. The shared sender validates the complete request and retains the draft when admission is rejected. `/references` reviews or removes attached snapshots; the footer shows their count. Frozen text, source identity, instruction snapshot and explicitly selected image bytes survive undo/redo, local draft recovery, uncertain submission recovery and queue withdrawal even if the source later changes or disappears.

Reference detail preparation, line wrapping, full instruction comparison, budget estimation and snapshot freezing run in the background. Results are bound to the requested source and generation; cancelled or superseded results cannot replace the current picker. Resize reflows cached details in the background while input remains available.

## Product management

The history picker shows a saved name or first-input title, latest work time and a short preview. `f` cycles Recent, All saved and Trash. Recent hides old initialization-only histories; named, forked and damaged histories remain discoverable. Search covers titles, previews and tags. `e` opens details with identity, full path, status, read-only access, branch inspection and reviewed migration copies. `r` renames a history without changing its activity time. Cold renaming holds the writer lock without starting plugins.

`d` reviews moving the selected history to Trash. The confirmation names the history and describes whether its live owner or active work must stop. Apply rechecks the selected identity, file revision, owner incarnation and workload; a changed selection requires another review. Closing the confirmation changes nothing. Current-session removal starts a new draft and transfers unsent input only after that draft is ready. Trash survives restarts and `r` or Enter restores the complete original records without overwriting a different history. Histories and Trash are never automatically purged.

Catalog results arrive incrementally, and newer requests or closing the picker cancel its directory reader. Refresh keeps the current filter and selected identity. Resuming a live owner attaches to it; stopped history acquires the normal writer lock. Neither path starts a new prompt. Damaged data remains available for inspection or source-preserving recovery. Binding mismatch provides a plugin-free reader and explicit migration to a new copy. `/tree` navigates the current history, optionally summarizing first; shared Session admission rejects navigation while busy.

`/models` distinguishes catalog visibility from configured access. Select a model to apply it to the current session. “Save as global default” also saves the choice for new sessions; a failed default write explicitly reports that the current model was already applied. Cold startup resolves the same default that the next request uses. Cycle visits configured models. Refresh and source changes preserve a run's frozen target. Selection requires an idle session, matching `eden models select`. `/auth` exposes an explicit API key entry and queries the provider for available browser/device methods. OAuth methods requiring client admission are offered only when that admission is configured. It can also refresh supported OAuth credentials or log out. The challenge menu offers the URL, code, expiry, browser opening, copying, private input, status and cancellation. Browser failures retain the copyable URL. Private input is cleared on submission and is never saved with a composer draft or public request receipt. After a lost private-input reply, inspect the original operation before entering material again.

`/router` lists observed remote model state and supports search, download, load, unload, cancellation and reconnect. Admission is separate from completion. Unknown progress stays unknown, and cancelling a local operation does not claim that the remote action was rolled back. Reconnect observes the remote state without replaying a mutation.

`/settings` selects the shared configuration form for an instance. `/plugins` also offers install/update, removal, composition resolution and instance replacement through the existing package and configuration services. Installation does not enable a package; building code requires its explicit build option. Replacement uses the reviewed configuration revision and the selected wait/cancel boundary. Schema-driven controls and the private-input channel retain the same revision, validation, preview and recovery behavior as `/config INSTANCE` and `/live`.

`/resources` shows skills, templates and diagnostics and offers resource reload. `/trust` shows effective project trust and any startup override, then saves an explicit project decision. Startup trust flags retain precedence during the current host's lifetime. `/background` opens notes/cache-warmer configuration, observes warming status and auxiliary usage, cancels warming with cleanup, or enters the existing compaction/rebuild workflow. Neither optional plugin is enabled by opening a management view; cache benefits are not inferred from request counts.

`/delivery` opens complete histories and reading JSONL as distinct read-only documents and prepares a filtered, fixed reading JSONL preview. Reading files cannot be resumed; selected tool-output text is decoded for reading, tool images use the same thumbnail path as conversation images, unknown or undecodable content remains visible as source, and damaged tails preserve the readable prefix with a diagnostic. The preview can be read in full, saved to a new file, discarded, or explicitly shared. Saving retains the same preview for a subsequent share. Leaving the workflow releases unused host previews, including late preparation replies. Saving and sharing consume the exact prepared bytes, even if the conversation grows. The share confirmation explains the secret-gist visibility. `/updates` separates installed-channel discovery, network checks, preparation and explicit activation; running sessions retain their bindings. `--offline-startup` suppresses automatic startup networking while keeping later explicit actions available.

Management dialogs use the existing keyboard and mouse controls. Ctrl+S applies a form; Escape returns from a form or detail to its management menu, then closes the menu. Page Up/Down scroll complete operation details. Pending mutations keep their original request identity; Ctrl+R reconciles an uncertain public submission before resend. Late replies do not reopen a closed page or overwrite a different page.

## CLI and terminal mapping

| CLI surface | Terminal consumer |
| --- | --- |
| No prompt on an interactive stdin/stdout | Default Focus TUI |
| Positional prompts, stdin, `--print`, `--json`, `--continue`, `--history`, RPC | Explicit batch/history/machine paths retain their existing behavior |
| `--composition`, `--cwd`, `--global-dir`, repeatable `--env-file` | New/restored host startup; an explicit endpoint retains its existing host configuration |
| `--session` / `--resume`, `--no-session`, `live`, `resume-live`, `tui --read`, `read` | Explicit new, resumed, attached or read-only session; `/sessions` manages saved histories |
| `--model`, `--thinking`, `models`, `auth`, `router` | Startup selection; `/models`, `/auth`, `/router` for live management |
| `--tools`, `--exclude-tools`, `--read-only` | Shared coding startup configuration and configuration forms; ordinary tool execution uses the same service decisions |
| `--skill-path`, `--template-path`, `--no-context`, `--no-skills`, `--no-templates` | Shared resource discovery; composer completion, `/resources`, configuration and explicit reload |
| `--trust-project`, `--no-trust-project` | Host-lifetime trust override; `/trust` inspects precedence and saves project decisions |
| `--offline-startup`, `--no-update-check` | Shared startup maintenance; `/updates` performs later explicit actions |
| `--attach`, `--image`, `--file` | Batch input remains explicit; TUI uses `/attach`, `@file`, clipboard and captured attachment snapshots |
| `--color`, `NO_COLOR` | Local terminal color policy; `/style` changes display preferences |
| `--quiet`, repeatable `--verbose` | CLI diagnostics; the detached host writes its diagnostics to its separate log |
| `session` tree/branch/fork/clone/metadata, queue and shell | `/sessions`, `/tree`, `/queue`, `/queue-mode`, `/shell`, `!` and `!!` |
| `resources`, `trust`, `package`, `config` | `/resources`, `/trust`, `/plugins`, `/settings` and the shared forms |
| `export`, `share`, `update`, `read` | `/delivery`, `/updates` and explicit read-only file opening |

Startup-only process paths, environment and composition selection remain explicit launch choices. Live configuration changes use the shared configuration service and its declared application boundary rather than mutating launch arguments.

## Overlay replacement

`EDEN_TUI_OVERLAY` selects an explicitly trusted native library exporting `eden_overlay_v1` from `eden-ui-sdk`. It receives the actual modal items, selection, query, masked field display values and semantic theme, and can render cells and map keys to validated host navigation. Host action targets and mouse hit regions remain authoritative. A rejected frame or event falls back to the built-in interface. The Overlay table has its own full-size header check, which covers the nested renderer prefix as well as the event slot; `export_ui!(overlay: Type)` writes that size, while a hand-written table must set it to the complete `OverlayApi` rather than to `RendererApi`. Renderer/editor/theme/frontend replacements continue to load independently. No Rust framework value or allocator crosses these C tables. [Native UI plugins](ui-plugins.md) documents the overlay trait, the reply contract and the failure values.

## Reproducing terminal measurements

Build with `cargo build --release --locked -p eden-cli -p eden-terminal-editor`, then run `python3 scripts/measure-tui.py`. The POSIX probe opens isolated committed histories with 1k, 10k and 100k messages, measures host startup and the first observed 120×36 PTY frame, and samples 20 composer edits. Its JSON report includes input-to-observed-text timings, emitted bytes and Linux process peak memory. The observation loop has 2 ms resolution; freshly generated files use normal filesystem caching. Add `--records 100000 --no-color --search` to verify the long-history monochrome search path. These measurements do not include a physical display or IME, and no model or account is involved.

The footer shows the latest request's reported uncached input, output and cache read/write usage, with each missing counter shown as `?`; reasoning usage is shown when reported. Ctrl+C retains the draft while interrupting an active run, including from a dialog; cancellation acceptance and confirmed cleanup are separate feedback states. Esc still closes a local surface first.

The bundled frontend reads an initial authoritative snapshot, then uses `/tui/snapshot?incremental=1` with event/presentation cursors and the active history head. Unchanged history is omitted on that opt-in wire path; an expired event cursor returns a complete reconstruction. Ordinary `/tui/snapshot` and older clients still receive full history. These transport updates do not change durable records, action receipts or native UI roles.

A controlled local SSE/real host/POSIX PTY regression is available as `python3 scripts/verify-tui-streaming.py --binary PATH --composition PATH --editor PATH --output DIRECTORY`. It exercises authentication, streaming after zero/three catalog changes, input, cancellation, actual tools, model selection/default scope and reopen. Its receipts are controlled-provider evidence, separate from authenticated external service or manual terminal acceptance.
