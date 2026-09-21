//! Model snapshots are safe to persist; credential payloads belong only to private calls.
use crate::coding::ModelLimits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// A replaceable catalog owns discovery and selection resolution.
pub const MODEL_CATALOG: &str = "eden.model-catalog.v1";
/// This contract carries secrets and must never be emitted or recorded.
pub const CREDENTIAL_SOURCE: &str = "eden.credential-source.v1";
/// Authentication interactions expose only redacted status to consumers.
pub const AUTH: &str = "eden.auth.v1";
/// A committed selection retains user intent independently from directory refreshes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub thinking: Option<String>,
}
/// Effective thinking can differ when the selected model lacks the requested level.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ThinkingSelection {
    pub requested: Option<String>,
    pub effective: Option<String>,
}
/// Capabilities are frozen with a run rather than reread midway through it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelCapabilities {
    pub images: bool,
    pub tools: bool,
    pub reasoning: bool,
}
/// Rates per million tokens are estimates, not account billing information.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelPricing {
    #[serde(default)]
    pub tiers: Vec<ModelPricingTier>,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub source: String,
}
/// Long-context rates apply only above their explicit input token threshold.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelPricingTier {
    pub input_tokens_above: u64,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}
/// Provenance identifies the data that supplied routing and limits.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CatalogSource {
    pub kind: String,
    pub location: String,
    pub updated_at: Option<u64>,
}
/// A run consumes one immutable, secret-free route for inference and summaries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelTarget {
    pub provider: String,
    pub model: String,
    pub api: String,
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub limits: ModelLimits,
    #[serde(default)]
    pub thinking: ThinkingSelection,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    pub pricing: Option<ModelPricing>,
    pub source: CatalogSource,
    /// Non-secret protocol compatibility metadata from the selected catalog.
    #[serde(default)]
    pub compat: serde_json::Value,
}
/// Listing distinguishes implemented routes from credentials and actual live verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CatalogEntry {
    pub target: ModelTarget,
    pub name: String,
    pub status: String,
}
/// Source changes invalidate in-flight publication without altering saved selections.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum CatalogRequest {
    List,
    Refresh,
    SetSource { url: String },
    Resolve { selection: Option<ModelSelection> },
    SetDefault { selection: ModelSelection },
}
/// Failure status may accompany a usable retained catalog.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CatalogReply {
    /// Includes dynamic identities whose models are not available in an offline snapshot.
    #[serde(default)]
    pub providers: Vec<String>,
    pub models: Vec<CatalogEntry>,
    pub target: Option<ModelTarget>,
    pub source: CatalogSource,
    pub status: String,
}
/// Private input: callers must not trace or persist explicit credentials.
#[derive(Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CredentialRequest {
    pub provider: String,
    #[serde(default)]
    pub explicit: Option<String>,
    #[serde(default)]
    pub purpose: String,
}
/// Private result deliberately omits Debug; only a trusted provider may consume it.
#[derive(Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CredentialReply {
    pub api_key: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub source: String,
    /// Authentication may bind an account to a private inference endpoint.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Subscription policy can restrict an otherwise discoverable catalog.
    #[serde(default)]
    pub available_model_ids: Option<Vec<String>>,
    /// Opaque account scope separates private dynamic catalog caches across logins.
    #[serde(default)]
    pub catalog_scope: Option<String>,
}
/// Input may contain a key, so this request is private and deliberately lacks Debug.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum AuthRequest {
    /// Start a provider login; method is browser or device (provider default when absent).
    Login {
        provider: String,
        method: Option<String>,
    },
    /// Drive callback or device polling until completion; scope cancellation closes its I/O.
    Wait {
        operation_id: String,
    },
    /// Supply a browser authorization code or redirect through the private contract.
    Submit {
        operation_id: String,
        input: String,
    },
    /// Refresh a managed credential without returning it to the caller.
    Refresh {
        provider: String,
    },
    Start {
        provider: String,
    },
    Input {
        operation_id: String,
        api_key: String,
    },
    Cancel {
        operation_id: String,
    },
    Status {
        operation_id: String,
    },
    Logout {
        provider: String,
    },
}
/// Challenges and outcomes contain no authentication material.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct AuthReply {
    pub operation_id: Option<String>,
    pub provider: String,
    pub status: String,
    pub challenge: Option<String>,
    /// User-facing login information, never a device secret, token or PKCE verifier.
    #[serde(default)]
    pub interaction: Option<AuthInteraction>,
    pub source: Option<String>,
}

/// Only transient login UI should display these values; do not persist them in history.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct AuthInteraction {
    pub url: String,
    pub user_code: Option<String>,
    pub manual_input: bool,
    pub expires_at: u64,
}

/// Optional model lifecycle service, independently replaceable from catalog and inference.
pub const MODEL_MANAGER: &str = "eden.model-manager.v1";

/// Mutations wait for observed remote completion; cancellation never implies remote success.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ManagerRequest {
    List,
    /// Requery remote state without replaying any mutation.
    Reconnect,
    Search {
        query: String,
    },
    Download {
        model: String,
    },
    Load {
        model: String,
        #[serde(default)]
        unload_others: bool,
    },
    Unload {
        model: String,
    },
    /// Stop a named remote load/download, including one started by a previous process.
    Cancel {
        model: String,
    },
}

/// Remote state and its inference projection share one observation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ManagedModel {
    pub id: String,
    pub state: String,
    pub source: String,
    pub failed: bool,
    pub selectable: bool,
    pub progress: Option<f64>,
    pub target: ModelTarget,
}

/// Search returns repository identities, suitable for an explicit repo:quant request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelSearchResult {
    pub id: String,
    pub downloads: Option<u64>,
    pub gated: bool,
    #[serde(default)]
    pub quants: Vec<ModelQuantization>,
}

/// A disconnected observation must never project cached models as currently selectable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ManagerReply {
    pub status: String,
    pub models: Vec<ManagedModel>,
    pub results: Vec<ModelSearchResult>,
    pub autoload: bool,
    pub max_instances: Option<u64>,
}

/// Repository GGUF variants group all shards; unknown file sizes keep total bytes unknown.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ModelQuantization {
    pub name: String,
    pub bytes: Option<u64>,
    pub files: Vec<String>,
}
