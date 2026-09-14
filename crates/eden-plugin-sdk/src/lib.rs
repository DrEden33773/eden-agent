//! Rust author API and exact-version C ABI for trusted native plugins.
pub mod abi;

pub mod author;
#[doc(hidden)]
pub mod runtime;
pub mod scope;
pub use author::{AgentLoop, CallContext, ContextStrategy, ModelProvider, Package, Tool};
pub use eden_protocol as protocol;
pub use scope::{Cancellation, Scope};
pub use serde_json;
pub use tokio;

/// Export the native entry point from a cdylib. Functions return metadata and construct roles.
#[macro_export]
macro_rules! export_plugin {
    ($descriptor:path, $factory:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_plugin_v1() -> *const $crate::abi::Header {
            unsafe extern "C" fn describe(reply: $crate::abi::Reply) {
                // SAFETY: The loader provides a live receiver for this synchronous descriptor.
                let result = std::panic::catch_unwind($descriptor).map_err(|_| {
                    $crate::protocol::Fault::new(
                        "PluginFailure",
                        "descriptor",
                        "descriptor panicked",
                    )
                });
                // SAFETY: The loader consumes the descriptor result synchronously.
                unsafe {
                    reply.send(&result);
                }
            }
            unsafe extern "C" fn create_shim(
                host: $crate::abi::HostApi,
                bytes: $crate::abi::Bytes,
                reply: $crate::abi::Reply,
            ) -> usize {
                // SAFETY: The loader owns host callbacks and the borrowed configuration span.
                unsafe { $crate::runtime::create($factory, $descriptor, host, bytes, reply) }
            }
            static API: $crate::abi::Api = $crate::abi::Api {
                header: $crate::abi::Header {
                    magic: *b"EDENABI\0",
                    abi: 1,
                    size: std::mem::size_of::<$crate::abi::Api>() as u32,
                    sdk: $crate::abi::field($crate::protocol::CONTRACT),
                    target: $crate::abi::field($crate::abi::TARGET),
                },
                describe,
                create: create_shim,
                start: $crate::runtime::start,
                cancel: $crate::runtime::cancel,
                release: $crate::runtime::release,
                destroy: $crate::runtime::destroy,
            };
            &API.header
        }
    };
}
