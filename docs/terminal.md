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

## Rich content

The terminal renders Markdown, highlighted code, tables and responsive split/unified Diff. Supported Mermaid flowcharts and sequence diagrams are laid out as terminal text. Common LaTeX symbols, fractions, roots and scripts are rendered natively in inline, display and fenced formulas. Unsupported or incomplete syntax retains readable source. Displayed content cannot emit terminal control sequences.
