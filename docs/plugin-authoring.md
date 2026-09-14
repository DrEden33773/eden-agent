# Author a native plugin

Use the SDK source from the same eden-agent release and Rust 1.98.1. Host and plugin must match `eden-native-0.1.0`, ABI version 1 and the target triple. There is no cross-release ABI compatibility promise.

## Independent build

The executable is built first. A plugin author needs the two public crates `eden-plugin-sdk` and `eden-protocol` plus their Cargo workspace dependency declarations; no host, kernel or first-party implementation source is required. The verifier assembles exactly this SDK source tree. When using a complete public clone, the sample manifests already point to the SDK and can build independently:

```sh
cargo build --manifest-path tests/contract-authors/loop-a/Cargo.toml --locked
cargo build --manifest-path tests/contract-authors/context-b/Cargo.toml --locked
```

Each sample declares its own Cargo workspace and lockfile. A new author project declares a `cdylib` library and a path dependency on the matching SDK. Implement `AgentLoop`, `ContextStrategy`, `ModelProvider` or `Tool` with ordinary Rust async methods; register implementations with `Package`. Use `export_plugin!(descriptor, create)` to generate the C ABI adapter. The descriptor's role list must match the package's registered roles and order.

For example, the loop calls `cx.context(&input).await`, `cx.model(&model_input).await`, and `cx.tool(argument).await` through host-selected services. It chooses their order. A context implementation returns `ModelInput`; its text reaches the provider unchanged by the kernel. Multiple roles in one package remain independently selectable.

## Install and select

Copy the compiled `.dll`, `.so` or `.dylib` into a new version directory in the installation. Append a package manifest to the explicit composition file with `descriptor`, `host`, `sdk`, `target`, `library` and `config`. Use the SDK's `abi::TARGET` or `rustc -vV` to obtain the target triple. Paths are relative to the composition file.

```json
{
  "descriptor": {
    "package": "loop-a",
    "version": "0.1.0",
    "provides": ["eden.agent-loop.v1"]
  },
  "host": "eden-native-0.1.0",
  "sdk": "eden-native-0.1.0",
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
