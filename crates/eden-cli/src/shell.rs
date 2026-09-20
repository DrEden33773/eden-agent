//! Single outlet for human-readable messages.
//!
//! Results stay on stdout so a caller can parse them; every human-readable
//! line leaves through here on stderr. Collecting them in one place is what
//! lets one slice give all of them the same level, color and verbosity rules
//! instead of each call site repeating its own `eprintln!`.
use std::fmt::Display;

/// One process's message outlet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shell;

impl Shell {
    /// Create the outlet for this process.
    pub fn new() -> Self {
        Self
    }
    /// A diagnostic produced while a workspace or snapshot was assembled.
    pub fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
    /// A status line about the work in progress, such as the session file.
    pub fn status(&self, message: impl Display) {
        eprintln!("{message}");
    }
    /// A failure that ends the process.
    pub fn error(&self, message: impl Display) {
        eprintln!("{message}");
    }
}
