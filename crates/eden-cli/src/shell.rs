//! Single outlet for human-readable messages.
//!
//! Results stay on stdout so a caller can parse them; every human-readable
//! line leaves through here on stderr. Collecting them in one place is what
//! lets one slice give all of them the same level, color and verbosity rules
//! instead of each call site repeating its own `eprintln!`.
use eden_protocol::resources::{Diagnostic, Level};
use std::fmt::Display;

/// One process's message outlet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shell;

impl Shell {
    /// Create the outlet for this process.
    pub fn new() -> Self {
        Self
    }
    /// A resource diagnostic, rendered at the level its producer chose.
    pub fn diagnostic(&self, diagnostic: &Diagnostic) {
        match diagnostic.level {
            Level::Warning => self.warn(&diagnostic.message),
            Level::Error => self.error(&diagnostic.message),
            _ => self.note(&diagnostic.message),
        }
    }
    /// A status line about the work in progress, such as the session file.
    pub fn status(&self, message: impl Display) {
        eprintln!("{message}");
    }
    /// Something worth knowing that is not a problem.
    pub fn note(&self, message: impl Display) {
        eprintln!("note: {message}");
    }
    /// Something was skipped or is worth attention.
    pub fn warn(&self, message: impl Display) {
        eprintln!("warning: {message}");
    }
    /// A failure, whether it ends this process or one operation in it.
    pub fn error(&self, message: impl Display) {
        eprintln!("error: {message}");
    }
}
