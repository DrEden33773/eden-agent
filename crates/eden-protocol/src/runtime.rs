//! Instance-bound routing and host-owned work; all values cross the byte ABI.
use crate::{Event, Terminal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
/// Host service for delegation, jobs and cancellable event subscriptions.
pub const HOST: &str = "eden.runtime.v1";
/// Optional activation service called after all instance publications are ready.
pub const READY: &str = "eden.instance-ready.v1";
/// Stable instance identity and one non-reusable incarnation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct InstanceIdentity {
    pub id: String,
    pub generation: u64,
}
/// Host-issued invocation identity. Retained identities expire at the cleanup barrier.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CallIdentity {
    pub owner: InstanceIdentity,
    pub scope: String,
    pub call: u64,
    pub next: Option<u64>,
    pub job: Option<u64>,
}
/// Empty graphs preserve legacy package-named instances and flat role selection.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Graph {
    #[serde(default)]
    pub instances: Vec<InstanceSpec>,
    #[serde(default)]
    pub scopes: BTreeMap<String, ServiceScope>,
}
/// An enabled instance of an installed package. Ownership and dependencies govern teardown;
/// its service scope chooses bindings independently of configuration layering.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct InstanceSpec {
    pub id: String,
    pub package: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub config: Option<Value>,
}
/// A scope inherits unresolved contracts from its parent. The empty id is the session root.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ServiceScope {
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub bindings: BTreeMap<String, Binding>,
}
/// Wrappers run in listed order before the explicit default tail.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Binding {
    pub tail: String,
    #[serde(default)]
    pub wrappers: Vec<String>,
}
/// Jobs call a declared service on the submitting instance with run id zero. Acceptance
/// returns an id before completion; Join returns only after all native cleanup settles.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum HostRequest {
    Environment,
    Call {
        scope: String,
        contract: String,
        input: Value,
    },
    Delegate {
        token: u64,
        input: Value,
    },
    Submit {
        contract: String,
        input: Value,
    },
    Inspect {
        job: u64,
    },
    Forget {
        job: u64,
    },
    Cancel {
        job: u64,
    },
    Join {
        job: u64,
    },
    Events {
        after: u64,
        kinds: Vec<String>,
    },
}
/// Job completion includes cleanup failures instead of reporting early cancellation as success.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct JobStatus {
    pub id: u64,
    pub owner: InstanceIdentity,
    pub terminal: Option<Terminal>,
}
/// A subscription cursor advances over filtered observations too, preventing a busy loop.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct EventBatch {
    pub cursor: u64,
    pub events: Vec<Event>,
}
