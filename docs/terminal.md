# Interactive terminal

Run the installed `bin/eden` in a terminal to start a new task. `eden tui` is the explicit equivalent. The terminal attaches to a separate local Session host, so closing the frontend leaves accepted work running. The exit message prints the exact endpoint to reconnect. `--print`, `--json`, piped input, explicit prompt runs and RPC keep their existing output and lifecycle.

```sh
eden tui
eden tui --endpoint /path/to/host.json
eden --session /path/to/history.jsonl tui
eden tui --read /path/to/history.jsonl
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
| Copy selection / clear unselected composer | Ctrl+C |
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

See the [SDK](../crates/eden-ui-sdk/src/lib.rs) and [independent author](../tests/contract-authors/tui-editor/README.md). `python3 scripts/verify-tui.py` checks the installed boundary; POSIX runs use a real PTY. Core/editor/renderer tests also run natively on the supported CI platforms.

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
