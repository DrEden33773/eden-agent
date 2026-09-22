//! Semantic extension interaction; authentication keeps its separate private channel.
use serde::{Deserialize, Serialize};
use serde_json::Value;
/// Host requests and notifications, available to native and in-process contributions alike.
pub const HOST: &str = "eden.interaction.v1";
/// A headless client can render these requests without implementing terminal components.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Interaction {
    Request {
        kind: String,
        title: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        initial: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Notify {
        kind: String,
        value: Value,
    },
}
