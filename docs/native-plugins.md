# Native plugin contract

The current development release pairs host and SDK exactly as `eden-native-0.1.0`, ABI version 1, on the same target triple. Authors use Rust 1.89.0. The host and each library own their Rust dependencies and runtimes. Libraries are trusted native code running with the host's permissions.

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

This version provides an in-memory controlled run, native role selection and author validation. The controlled provider and tool make model input and tool-result flow reproducible without credentials or file modifications. Persistent history, actual providers, coding tools and terminal UI are subsequent product work.
