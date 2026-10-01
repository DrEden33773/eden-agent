# Native terminal frontend

`grok/` retains the Apache-2.0 Grok Build layout, editor, rendering, scrolling and tool/Diff components from reference `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`, recorded in `grok/UPSTREAM_COMMIT`. The original `grok/SOURCE_REV` is an upstream internal identity and is recorded separately. Eden changes are tracked source. The application is a library called by `eden-terminal` inside the installed `eden` process; accepted work belongs to independent Eden Session hosts.

Build with Rust 1.98.1 and the root Cargo.lock:

```sh
python3 scripts/build-candidate.py /absolute/installation
/absolute/installation/bin/eden --cwd /absolute/project
```

The root workspace builds the terminal and host together. The imported crates retain their source workspace manifest for dependency declarations, but installation uses one root dependency resolution. The checked-in tools API types avoid a protobuf compiler download. The fixed rendering dependency adjustment is documented in [vendor/ratatui-0.29/EDEN.md](../vendor/ratatui-0.29/EDEN.md).

`SessionWorkspace` owns opening, identity checks, catalog operations, Trash and attachment cleanup. The frontend uses in-process commands and ordered events. New tasks remain drafts until the first accepted work or explicit naming publishes history. `/resume` and `/sessions` share one picker with Recent, All saved and Trash views. Ordinary frontend exit preserves accepted host work; unused drafts and owned readers are closed.

`grok/LICENSE`, `grok/THIRD-PARTY-NOTICES` and the product notices preserve attribution. `CANDIDATE.json` records the source fingerprint and installed bytes against their root build outputs. Runtime does not build or prepare source. The compatibility `build-frontend.py` command builds `eden-cli` from the root workspace.
