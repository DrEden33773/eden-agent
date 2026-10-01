# Retained protocol bindings

`src/generated.rs` is the existing accepted frontend build output for `proto/grok-tools.proto`, generated with the fixed reference protobuf toolchain and the serde attributes from the imported build script. Eden retains it as source so a normal Cargo build does not download or execute another build-time tool. The protocol schema and its Apache-2.0 ownership are unchanged. `tests/wire_shape.rs` checks the externally used JSON shapes.
