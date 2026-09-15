# Resources, trust and package composition

These facilities are ordinary native plugins consumed through the shared `Session` API. The default installation includes resource discovery, command and hook routing, package preparation, and FFF search. The kernel continues to route explicit namespaced service strings; it does not enumerate third-party domains.

## Settings and trust

Global state defaults to `~/.eden/agent`, overridden by `EDEN_AGENT_DIR` or `--global-dir`. Global `settings.json` is merged with trusted `<cwd>/.eden/settings.json`, then explicit CLI overrides. Objects merge recursively; arrays and scalars replace. Configured `skills` and `templates` paths resolve relative to the settings file's directory. CLI `--skill-path` and `--template-path` resolve relative to cwd.

```sh
eden trust allow /path/to/project
eden trust deny /path/to/project/untrusted-child
eden trust inspect /path/to/project
eden --cwd /path/to/project --trust-project "Work on this project"
eden --cwd /path/to/project --no-trust-project "Inspect ordinary files"
```

Trust records bind canonical directories and their descendants; the closest explicit grant or denial wins. Granting a parent requires naming that parent. A directory's ordinary configuration changes do not revoke its grant. `--trust-project` and `--no-trust-project` override trust for this invocation. Noninteractive startup ignores untrusted project settings and project Skills, templates and SYSTEM files, and emits a `resource_diagnostic`. Ordinary ancestor AGENTS text is still read. Trust is authorization to use project configuration and code, not an operating-system sandbox. Native packages are installed and enabled explicitly; entering a project never downloads, builds or discovers native libraries.

The `plugins` object contains configuration by declared package name. For example:

```json
{
  "tools": ["read", "write", "edit", "bash", "skill", "ls", "find", "grep"],
  "exclude_tools": [],
  "read_only": false,
  "plugins": {
    "search": { "persist_history": false, "allow_broad_scan": false },
    "coding-tools": { "bash": "bash", "powershell": "pwsh" }
  }
}
```

Host-derived resource cwd, trust and state directories cannot be replaced by plugin settings. `--tools` and `--exclude-tools` accept comma-separated names. `--read-only` removes write, edit and shell tools; third-party tool contributions must declare themselves read-only to participate. The default catalog contains read/write/edit/bash and the on-demand skill loader. `ls`, `find`, `grep` and native `powershell` are explicit selections. PowerShell uses no profiles, the session cwd, and the same process-tree cancellation barrier as Bash.

## Instructions, Skills and templates

Instruction discovery reads the global directory, then filesystem ancestors through cwd. Within each directory, the first of `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` wins. Paths are canonicalized to avoid duplicate sources. Global `SYSTEM.md` replaces the default system text; trusted `.eden/SYSTEM.md` has priority. `APPEND_SYSTEM.md` supplies appended system text.

Skills are discovered in global `skills/`, the shared `.agents/skills` location associated with the global home layout, trusted project `.eden/skills` and ancestor `.agents/skills` through the Git root, plus explicit paths. A skill uses Markdown with YAML frontmatter:

```markdown
---
name: check
description: Run the project's checks and report observed failures.
disable-model-invocation: false
---
Read ./checks.md before changing validation commands.
```

Discovery injects only the name, description and source path into model context. Bodies are retained in the immutable resource snapshot and are sent only when the model calls `skill` or the user submits `/skill:check arguments`. Relative references resolve from the skill directory. `disable-model-invocation: true` hides the skill from model invocation while retaining explicit user invocation. Duplicate names keep the first source and produce a diagnostic.

Templates come from global `prompts/`, trusted `.eden/prompts/`, and explicit paths. `/filename arguments` expands that Markdown template. Arguments support single/double quotes and escaping, `$1`, `$2`, `$@`, `$ARGUMENTS`, `${3:-default}`, `${@:2:1}` and `${ARGUMENTS:-default}`. Expansion never executes shell syntax.

`--no-context`, `--no-skills` and `--no-templates` disable their automatic discovery. Explicit skill/template paths remain explicit sources. `eden resources list` reports the active resource metadata. The shared API's `reload_resources()` is an idle management operation: it prepares a complete replacement before publishing it, and a failed reload retains the prior snapshot. Each coding run freezes one resource snapshot and tool catalog. Reopening reads current resources at the recorded cwd while retaining historical content that was actually sent to the model.

## FFF search

`find` and `grep` are provided by the search plugin using `fff-search 0.10.6` Rust libraries, with the Rust ignore walker and no dependency on an installed `rg` executable. The plugin owns a helper process containing FFF's scans, indexing threads and watchers. Cancellation and instance stop kill and wait for that process before returning; reopening creates a new instance and invalidates old cursors.

| Parameter | Behavior |
| --- | --- |
| `pattern` | Required, nonempty; no implicit FFF query-language operators |
| `path` | Default cwd; existing explicit file or directory, relative or absolute; missing paths fail without widening |
| `mode` | grep defaults to `literal`, with explicit `regex` and `fuzzy`; find defaults to whole-relative-path `fuzzy`, with explicit `glob` |
| `case` | `sensitive` by default; explicit `insensitive` or `smart` |
| `fallback` | Optional `fuzzy`, only for complete literal zero hits; exact evidence and candidates remain separate |
| `exclude` | Relative-path glob exclusions retained across modes and fallback |
| `limit` / `cursor` | Default 30, range 1–200; continue the same query, including within one file |
| `refresh` | Explicitly rebuild the in-memory index before a new query |
| `follow_symlinks` | Default false; explicit following deduplicates physical file/line identities and handles cycles |
| `max_file_bytes` | grep defaults to 5 MiB, maximum 10 MiB; omissions are reported |
| `ranking` | Default relevance: exact results use path/line order and fuzzy results use matching score/path; explicit `git`, `history`, or grep-only heuristic `definition` priority |

A file scope never scans its parent. Directory scans honor FFF's ignore policy, including `.gitignore`, `.ignore`, Git excludes, and non-repository cache-directory exclusions; `.git` is excluded. The index remains local to the explicit scope. Searching the home directory or filesystem root requires `plugins.search.allow_broad_scan: true`; this is configuration, not a model tool argument. Search access does not grant project code trust.

Results use a compact metadata header and one quoted path per adjacent group, followed by `line: text`. `complete` describes scope evaluation; `has_more` and `cursor` describe additional result or diagnostic pages. Incomplete results report `total_matches: null`, `matched_so_far` and explicit skipped reasons. Pages bound the combined match/diagnostic count and target 16 KiB of row data, while allowing one oversized diagnostic to remain fetchable. Long displayed lines carry a read continuation. FFF's fuzzy matcher can omit matches beyond its 512-byte line view; affected lines are explicitly reported as incomplete, with no exact total or implicit fallback proof. Regex errors, cancellation, partial evaluation and continuation pages never silently change mode.

Cursors bind query semantics and searchable file revisions. Relevant changes, reopen or bounded cache eviction produce a stale-cursor error, never a silent restart. Changes to ignored content alone do not invalidate a page. The index is rebuilt when the current searchable file inventory or metadata differs, supplementing the native watcher so the next query sees write/edit effects. There is no promise of recovery of a disk content index.

Access learning is disabled by default. `plugins.search.persist_history: true` enables an Eden-owned JSONL history under the global `search-history/` directory, separated by canonical project cwd. Only successful read-tool accesses are recorded; a returned search hit alone is never recorded as a read. A read of a previously displayed result also records the associated query. Explicit `history` ranking prioritizes decayed access counts, with a seven-day half-life and extra weight for the matching query, then retains the normal order for ties. The default order remains independent of this history.

## Package sources and locked versions

Package preparation is an explicit command contributed by `distribution`. It requires a bundle with root `package.json` containing `manifest`, optional `dependencies`, and optional `build`. An archive may have one enclosing directory. `manifest` is the native `PackageManifest`; `library` is relative to the bundle root. Dependencies are source objects with the same forms below. Package links and special files are rejected.

```sh
eden package install /path/to/bundle
eden package install /path/to/bundle.tar.gz
eden package install --source-json '{"kind":"git","url":"https://example.org/plugin.git","revision":"EXACT_COMMIT"}'
eden package install --source-json '{"kind":"https","url":"https://example.org/plugin.tar.gz","sha256":"ARCHIVE_SHA256"}'
eden package install /path/to/source-bundle --build
eden package list
eden package remove example 1.0.0
```

Git revisions are resolved and locked to an actual commit. HTTPS archives require a digest and reject redirects to HTTP. For a private CA, explicit `plugins.distribution.ca_certificate` names an absolute PEM file added to platform trust; verification remains enabled. GitHub Release assets use the HTTPS source form.

Source builds require `--build` and a `build` object with `manifest_path` and `artifact`, both relative paths inside the source bundle. Cargo metadata must identify one package containing a cdylib target. The command builds that package with `--release --locked --lib --package`; it does not rebuild the installed host. Missing target artifacts never trigger an implicit build. All dependencies are staged and checked before publishing; cancellation before publication does not install a version. Once publication begins, the transaction finishes or rolls back its new directories.

Installed versions coexist under global `distribution/packages/<name>/<version>/<target>`. Receipts lock the source, manifest, dependencies and an unambiguous SHA-256 tree digest. The same version cannot be overwritten with different bytes. Resolve and session load verify managed installed packages against their receipt. Saved composition references and actual saved session references are reported before removal; `--force` explicitly removes a referenced version while preserving history files.

```sh
eden package resolve '{"base":"/path/to/composition.json","packages":[{"name":"example","version":"1.0.0"}],"roles":{"example.service.v1":"example"}}'
eden session switch /path/to/session.jsonl --composition /path/to/resolved-composition.json
```

Resolve produces an immutable composition without downloading missing dependencies. Manifest `requires` contains exact contract strings that must be selected in that composition. Sessions bind the declared package versions, service selection, target and native library hashes. Resource/configuration text can change on reopen; changing a native binding requires an explicit switch. Running sessions reject switches. Preflight failure keeps the old generation available; once the old generation stops, a new initialization failure leaves the session unavailable until explicit recovery. Old native handles reject subsequent calls. CLI switch is also the explicit recovery entry for saved sessions whose prior composition is no longer available.

## Independent contributions

A plugin can publish arbitrary string contracts using `Package::service` and call selected contracts using `CallContext::call`. Its `requires` list names dependencies exactly, including version suffixes. Host Rust enums and recompilation are unnecessary. `tests/contract-authors/service-a` and `service-b` independently exercise this bridge after the verifier fixes the installed host.

`coding-tools.config.contributions` lists `{ "catalog": "author.catalog.v1", "execute": "author.execute.v1", "read_only": true }`. Duplicate tool names fail. The `contributions` plugin takes `commands` entries with catalog/execute strings plus ordered `input_hooks` and `tool_hooks`. Duplicate command names fail. Input hooks run after expansion and before model context; tool hooks run after original model intent is committed and before execution. Hooks cannot change session cwd, call identity or the input resource revision. Both original and transformed tool intents are committed before effects.

`eden commands` lists commands; `eden command example.compute '{"value":7}'` invokes a contributed command through shared session ownership. These CLI surfaces are the noninteractive slice; full terminal UI work is separate.
