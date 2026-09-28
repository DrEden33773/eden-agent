//! Private configuration input is carried separately from shared presentation actions.
use crate::{configuration_form::Edit, presentation::ActionRequest};
use serde::{Deserialize, Serialize};

/// Only the designated configuration receiver consumes these materials. Never log or cache this body.
/// The public request identity is immutable: retries may omit inputs and recover the original result.
#[derive(Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Submission {
    pub request: ActionRequest,
    pub inputs: Vec<Edit>,
}
impl std::fmt::Debug for Submission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateSubmission")
            .field("inputs", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn debug_does_not_disclose_input() {
        let input: super::Submission = serde_json::from_value(serde_json::json!({
            "request": {
                "session_id": 1,
                "owner": "receiver",
                "view_id": "settings",
                "revision": 1,
                "action": "settings:apply",
                "request_id": "one",
                "values": {},
            },
            "inputs": [{ "operation": "set", "path": "/key", "value": "PRIVATE_CANARY" }],
        }))
        .unwrap();
        assert!(!format!("{input:?}").contains("PRIVATE_CANARY"));
    }
}
