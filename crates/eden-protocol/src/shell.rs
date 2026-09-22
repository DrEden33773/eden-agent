//! Explicit user shell operations are independent of the model tool catalog.
use crate::coding::ToolResult;
use serde::{Deserialize, Serialize};

/// Runs a user-requested command with the session's process cleanup barrier.
pub const USER_SHELL: &str = "eden.user-shell.v1";
/// Optional trusted hook; receives a [`ShellRequest`] before process admission.
pub const BEFORE_USER_SHELL: &str = "eden.before-user-shell.v1";

/// The shell is `bash` or `powershell`; cwd belongs to the session and cannot
/// be changed by a hook. There is no implicit execution deadline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ShellRequest {
    pub cwd: String,
    pub command: String,
    pub shell: String,
}
/// A hook may rewrite the command or shell, or supply a result without spawning.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ShellHookReply {
    Execute { request: ShellRequest },
    Return { result: ToolResult },
}
/// Payload of `user_shell_output`. Byte chunks preserve split UTF-8 sequences
/// and binary output; consumers decode incrementally per stream and run ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ShellOutput {
    pub stream: String,
    pub bytes: Vec<u8>,
}
