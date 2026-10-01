# Native terminal frontend

`grok/` is the fixed Apache-2.0 Grok Build source at `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`, imported from the locally accepted pager source at Eden `57836744e2664f9359aa649b5f963c0e45e0035b`. It retains the layout, editor, rendering, scrolling and tool/Diff components. Eden modifications are ordinary tracked source; building never runs the experiment preparation script.

Build with Rust 1.98.1. The build helper downloads the fixed reference protobuf compiler when `PROTOC` or a compiler on PATH is unavailable:

```sh
python3 scripts/build-candidate.py /absolute/installation
/absolute/installation/bin/eden --cwd /absolute/project
```

The CLI selects a Session through `eden-session-lifecycle`, then starts `bin/eden-frontend`. The frontend embeds `eden-frontend-session` for typed host communication, projection and request/run/attempt reconciliation. There is no runtime Python wrapper or external ACP adapter. Native hosts retain accepted work after terminal detach. `/resume` selects a history explicitly; `/sessions` exposes read-only inspection and migration copies. No-target startup creates a persistent empty Session immediately.

`grok/LICENSE`, `grok/THIRD-PARTY-NOTICES` and the product notices preserve upstream attribution. This is an independent Cargo workspace because the fixed rendering stack uses a different ratatui version. Its checked-in lockfile owns that stack; Eden path libraries retain the root workspace's dependency and lifecycle contracts.

`CANDIDATE.json` binds both successful builds to the same source fingerprint and records the installed bytes against their build outputs. `EDEN_FRONTEND_TARGET_DIR` changes only the frontend build cache location; the source remains `frontend/grok/`. Native installed verification builds this frontend before assembling its frozen seed.
