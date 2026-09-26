# Native plugin contract

The current development release pairs host and SDK exactly as `eden-native-0.8.0`, ABI version 1, on the same target triple. Authors use Rust 1.98.1. The host and each library own their Rust dependencies and runtimes. Libraries are trusted native code running with the host's permissions.

## Installation and selection

The CLI reads an explicitly supplied composition JSON file, or `../composition.json` relative to its executable. Package paths resolve relative to that file. Entering a project directory never discovers or executes native code. A composition lists immutable package/version directories, exact host/SDK/target identity, exported roles, configuration, and one selected package for each required role. Missing or incompatible roles fail startup rather than choosing a fallback.

The loader validates all manifests before loading any library. It then reads the fixed C ABI header and checks magic, version, full table length, SDK and target before reading function pointers. The library's descriptor must equal the declared package metadata. Code remains resident for the process lifetime; stopping an instance does not unload its library.

## Calls and ownership

Only `repr(C)` records, integers, borrowed byte spans and C function pointers cross the library boundary. A span is valid only for the synchronous callback receiving it; the receiver copies data it needs later. Every allocating side frees its own data. Rust futures, trait objects, wakers, Tokio handles and Cordis types never cross the boundary.

An instance supports concurrent independent operations. Each operation owns one asynchronous root, registered children, cleanup and cancellation. Completion callbacks fire exactly once after cleanup. Host service requests use the same asynchronous callback contract. A dropped caller receiver does not cancel or erase the owner's work; explicit cancellation requests and shutdown provide the stop barrier.

The host assigns session and run identity. Role requests inherit this identity through the SDK context. Cordis publishes host-defined composition services and owns their disposal effects. A cached proxy carries instance admission state: stopping closes it before waiting, so a prior handle cannot admit new work.

## Root result and cleanup

The SDK fixes Completed, Failed or Cancelled when the root finishes or accepts cancellation. It then closes child admission, signals cancellation, joins registered children and attempts all registered cleanup futures. Cleanup errors are separate from the root outcome. A late cancellation cannot turn a prior failure into Cancelled. Host service bridges remain registered until their callback settles, even if the original caller has dropped its receiver.

Authors must register asynchronous work with the operation scope. Work created outside that scope remains the author's responsibility. Native blocking work must cooperate and be joined by registered cleanup. Unwind at supported Rust boundaries becomes a structured failure; aborts and memory corruption are not isolated.

The ordinary host shutdown path awaits run settlement, withdraws services and destroys instances on a blocking thread after their operations have completed. Each instance's plugin-owned runtime is shut down before that barrier returns. Separate sessions have separate native instances and cancellation ownership.

## Release scope

The default installation provides coding runs with OpenAI Responses, read/write/edit/bash, JSONL history, tree navigation, context compaction, explicit migration, resume and queues through native roles. Model and credentials are explicitly configured. The controlled skeleton remains an isolated lifecycle regression composition. See [coding sessions](coding.md) for current behavior; broader Provider authentication and terminal UI are subsequent product work.

## Instance finalization and authored services

An instance may additionally provide `eden.instance-stop.v1`. After public admission closes and ordinary operations drain, the host calls this finalizer once, awaits its cleanup, then destroys the instance. Finalizers must finish their own work without calling withdrawn services. Their failures are reported by shutdown, including repeated shutdown observations. Libraries without the service retain their prior destruction behavior. This is an additive string service; the C ABI table layout is unchanged.

Manifest `requires` lists selected dependency contract strings. The host validates those strings before loading code, including contracts it has never seen before. The independent service authors and `scripts/verify-workspace.py` demonstrate native author B calling author A through `CallContext::call`, command and hook contributions, ResourceSource replacement, explicit composition recovery and old-handle rejection. See [workspace facilities](workspace.md) for configuration and package sources.

A managed child may retain its final observational result with `Scope::retain_result`, including after root cancellation starts cleanup. The runtime places that value in `Terminal.partial_result` only for an interrupted outcome; it never converts cancellation to success. The completion barrier freezes this value along with cleanup results. Authentication projection removes retained values from public events.

## Instance routing and managed work

The optional composition `runtime` graph separates installed packages from instances. `runtime.instances` declares `{ id, package, scope, owner, dependencies, config }`. A package without explicit instances keeps its package-named legacy instance. Explicit instances may share one native library with different configuration; each has a distinct host-issued generation, runtime and disposal barrier. `owner` and `dependencies` are stable instance ids, and disposal runs dependents and children first. Libraries remain resident after disposal. Local, already-created SDK contributions supply one instance per package.

`runtime.scopes` maps scope ids to `{ parent, bindings }`; the empty id is the session scope. Each binding contains a `tail` instance id and optional ordered `wrappers`. Legacy `roles` supply session bindings when no explicit binding overrides them. A child inherits missing bindings from its parent; an override does not modify siblings. An instance's declared scope confines its calls: a descendant caller scope stays in that descendant, an ancestor caller enters the declared scope, and calls from isolated sibling scopes fail. Configuration inheritance does not create lifecycle ownership.

Ordinary `CallContext::call` and retained `Session::role` handles execute the selected wrapper chain. A wrapper calls `delegate` with the downstream input or returns directly to short-circuit. The host issues a single-use continuation valid only through that invocation's cleanup barrier. Reusing it fails with `ExpiredContinuation`; recursively calling the same instance/contract fails with `RecursiveCall`. Errors retain their downstream source. `call_in` can explicitly choose the current scope or a descendant. Observe-only plugins can use `events_after` to subscribe without replacing a service; cursors advance over filtered observations and report lag explicitly.

After every instance is published, the host invokes each declared `eden.instance-ready.v1` service. This activation may call `submit_job` to register a callback contract provided by that same instance. Registration returns a job id before completion. A job has independent cancellation and operation scope, runs with `run_id = 0`, and survives the submitting turn. It can await later turn events, call services and register real asynchronous cleanup. `inspect_job`, `cancel_job` and `join_job` distinguish observation, cancellation request and completed cleanup. Only the owning instance accesses its jobs; a job cannot join itself. `forget_job` releases a settled receipt; submission refuses more than 1024 retained receipts per instance, so long-running authors should forget results they have consumed.

`Kernel::quiesce_instance` closes admission and signals cancellation; `stop_instance` awaits the instance's jobs, operations, bridges and finalizer. Dropping a waiter does not abandon this work. Retained service handles and call identities cannot route to a replacement generation. Finalizers may call still-published declared dependencies; they cannot start new jobs or call withdrawn services. Configuration management uses these primitives for dependency-scoped replacement and one automatic recovery attempt; see [configuration management](configuration.md). Manifest `requires`, wrapper tails, explicit dependencies and children define the restart closure and reverse teardown order. Cycles in that combined dependency graph are rejected before loading code.

Every author can read `CallContext::host_environment()`. Object-shaped factory configuration also receives the same `__eden_host` envelope, decoded by `environment::HostEnvironment::from_config`. The Session replaces user-supplied envelope values after workspace discovery. Paths, trusted settings, resource packages and history location come from that host input; scalar/array configuration is preserved and can use the asynchronous getter after activation. The default model-access, search, distribution, coding-tools and workspace-resources plugins consume this public input rather than package-name injection.

Host/SDK pairing changed from `eden-native-0.7.0` to `eden-native-0.8.0`; rebuild native authors and manifests together. The C table remains ABI 1. Rust `Request` now has optional `execution` metadata, `Composition` has defaulted `runtime` and optional `host_environment`, and `Session::role` returns a generation-bound `ServiceHandle`. Legacy flat JSON compositions remain readable; stored composition bindings include graph identity without configuration values. Installed native acceptance lives in `tests/contract-authors/runtime`, `runtime_probe`, and `scripts/verify.py`, including a TCP acknowledgement that must precede job join completion.

`Kernel::composition()` returns an owned declaration snapshot because local configuration commits can replace it. `instance_service` captures an owner generation for delayed actions. `replace_instances` preserves unrelated publications and classifies initialization failure separately from a cleanup barrier failure; Session owns durable commit and recovery. Native code-version replacements use distinct immutable library paths. Routing topology or service-declaration changes continue to use explicit composition switching; already-created embedded contributions can support declared live updates but have no native recreation factory.
