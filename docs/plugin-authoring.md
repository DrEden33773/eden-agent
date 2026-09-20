# Author a native plugin

Use the SDK source from the same eden-agent release and Rust 1.98.1. Host and plugin must match `eden-native-0.2.0`, ABI version 1 and the target triple. There is no cross-release ABI compatibility promise.

## Independent build

The executable is built first. A plugin author needs the two public crates `eden-plugin-sdk` and `eden-protocol` plus their Cargo workspace dependency declarations; no host, kernel or first-party implementation source is required. The verifier assembles exactly this SDK source tree. When using a complete public clone, the sample manifests already point to the SDK and can build independently:

```sh
cargo build --manifest-path tests/contract-authors/loop-a/Cargo.toml --locked
cargo build --manifest-path tests/contract-authors/context-b/Cargo.toml --locked
```

Each sample declares its own Cargo workspace and lockfile. A new author project declares a `cdylib` library and a path dependency on the matching SDK. For the default coding combination, register typed async handlers with `Package::service` using the constants and payloads in `eden_protocol::coding`, and call selected roles through `cx.call`. The original `AgentLoop`, `ContextStrategy`, `ModelProvider` and `Tool` traits describe the controlled skeleton used by the native regression tests. Use `export_plugin!(descriptor, create)` to generate the C ABI adapter. The descriptor's role list must match the package's registered roles and order.

For example, the loop calls `cx.context(&input).await`, `cx.model(&model_input).await`, and `cx.tool(argument).await` through host-selected services. It chooses their order. A context implementation returns `ModelInput`; its text reaches the provider unchanged by the kernel. Multiple roles in one package remain independently selectable.

## Coding role contracts

The default roles are `eden.coding-loop.v2`, `eden.coding-context.v2`, `eden.coding-provider.v1`, `eden.coding-tool.v1`, `eden.session-store.v2` and `eden.submission-queue.v2`. `RunInput` carries cwd and owned multimodal blocks; the context maps `ContextInput` to `ModelInput`; a provider maps that input to complete `ModelReply` items while optionally emitting transient deltas. The tool receives `ToolRequest` and returns structured `ToolResult`. Storage accepts `StoreRequest` and returns `StoreReply` only after local public commit. See [coding sessions](coding.md) for ordering, recovery and queue semantics.

`Snapshot.diagnostics` carries one record per skipped resource: a `level` of `info`, `warning` or `error`, plus the human-readable `message`. The level enum is non-exhaustive, so read a level you do not know as informational instead of failing. `eden-native-0.2.0` is the release that introduced this element shape; a plugin built against `eden-native-0.1.0` is rejected at load rather than deserialized into the new records.

`Package::service` infers serialized input/output types from a handler, for example `Package::new("my-context").service(eden_plugin_sdk::protocol::coding::CONTEXT, project)`, where `project(input: ContextInput, cx: CallContext)` returns `Result<ModelInput, Fault>` asynchronously. Native memory ownership and scope cleanup are identical to the original role facades. Only one selected implementation supplies each role; adding an implementation does not alter the host.

The [coding replacement author](../tests/contract-authors/coding-replacements) independently replaces provider, context, tool and storage. Build it with its own manifest and lockfile. The installed coding verifier checks downstream requests, real tool results and independent readability of the replacement store's local history after unloading that composition.

## Install and select

Copy the compiled `.dll`, `.so` or `.dylib` into a new version directory in the installation. Append a package manifest to the explicit composition file with `descriptor`, `host`, `sdk`, `target`, `library` and `config`. Use the SDK's `abi::TARGET` or `rustc -vV` to obtain the target triple. Paths are relative to the composition file.

```json
{
  "descriptor": {
    "package": "loop-a",
    "version": "0.1.0",
    "provides": ["eden.agent-loop.v1"]
  },
  "host": "eden-native-0.2.0",
  "sdk": "eden-native-0.2.0",
  "target": "x86_64-unknown-linux-gnu",
  "library": "plugins/loop-a/0.1.0/libauthor_loop_a.so",
  "config": null
}
```

Change the composition's `roles["eden.agent-loop.v1"]` to `"loop-a"`. To mix in Context B, append its package descriptor and select `"context-b"` for `"eden.context.v1"`. Keep the standard provider and tool selected. The verifier's `artifacts/install/mixed.json` is a complete runnable example for the current platform.

An enabled package is initialized even if none of its roles is selected. Omit packages you do not want to initialize. There is no automatic source build, package discovery, project trust prompt, or library hot unload in this release.

## Managed asynchronous work

Every call receives `CallContext` with a `Scope`. Use `scope.spawn` for child tasks and `scope.cleanup` for asynchronous teardown. Children receive `scope.cancellation()` and must cooperate. The scope closes registration when the root settles, joins children, and attempts all cleanup futures before completion. A cleanup may await actual resource shutdown; cancellation does not bypass it. Emit observations during the active operation with `cx.emit`; emissions after its scope closes are rejected.

The SDK runs the operation on a plugin-owned Tokio runtime, so native async I/O does not depend on sharing the host's reactor. Root and cleanup errors preserve their source. The [lifecycle author](../tests/contract-authors/lifecycle) demonstrates real TCP waiting, managed child cancellation and an externally gated cleanup. See the [ABI contract](native-plugins.md) for memory ownership and supported unwind boundaries.

## Session and context extension contracts

`ContextInput.records` contains public history; the context role owns its model projection. `action` selects normal projection, explicit compaction or branch summarization. `ModelInput.max_output_tokens` optionally narrows a summary request without changing the ordinary model allowance. `eden.model-info.v1` returns non-secret model limits. Changed loop/context/store/queue semantics use v2 role names so earlier authors cannot silently accept incompatible requests.

SessionStore implements `Open`, `Append`, `AppendBatch`, `Navigate`, `Create`, `Read` and `Close`. An `AppendBatch` receipt certifies all entries together; a response and consumption cannot be partially acknowledged. Even alternative backends retain a local public v2 transaction log usable without their library. The public history module provides schema validation and transaction encoding, not a second host-owned writer. `Create` publishes a new independent history and refuses existing destinations.

Interpreter and migrator roles receive versioned `ExtensionState` records. Only current required state is a continuing dependency. A migrator must report preserved/lost information and return one converted state for each source record, in order, preserving branch ownership; preview and apply must agree. Target interpretation validates required converted state before destination creation. See the independent `coding-replacements` author for actual alternate store, context, interpreter and migrator implementations.
