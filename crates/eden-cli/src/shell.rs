//! Single outlet for human-readable messages.
//!
//! Results stay on stdout so a caller can parse them; every human-readable
//! line leaves through here on stderr. Collecting them in one place is what
//! lets every message share the same level, color and verbosity rules instead
//! of each call site repeating its own `eprintln!`.
use crate::style;
use clap::ColorChoice;
use clap::builder::styling::Style;
use eden_protocol::resources::{Diagnostic, Level};
use std::fmt::Display;

/// One process's message outlet.
#[derive(Clone, Copy, Debug)]
pub struct Shell {
    color: bool,
}

impl Shell {
    /// Create the outlet for this process.
    ///
    /// The decision is made once, so a redirected stderr cannot change the
    /// shape of the messages half way through a run.
    pub fn new(choice: ColorChoice) -> Self {
        use std::io::IsTerminal;
        Self {
            color: colored(choice, std::io::stderr().is_terminal()),
        }
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
    pub fn status(&self, label: &str, detail: impl Display) {
        self.line(style::HEADER, label, detail);
    }
    /// Something worth knowing that is not a problem.
    pub fn note(&self, message: impl Display) {
        self.line(style::NOTE, "note:", message);
    }
    /// Something was skipped or is worth attention.
    pub fn warn(&self, message: impl Display) {
        self.line(style::WARN, "warning:", message);
    }
    /// A failure, whether it ends this process or one operation in it.
    pub fn error(&self, message: impl Display) {
        self.line(style::ERROR, "error:", message);
    }
    /// One labeled line, with the label colored when this outlet colors.
    fn line(&self, style: Style, label: &str, detail: impl Display) {
        if self.color {
            eprintln!("{}{label}{} {detail}", style.render(), style.render_reset());
        } else {
            eprintln!("{label} {detail}");
        }
    }
}

/// Whether a stream that is or is not a terminal is written with color.
fn colored(choice: ColorChoice, terminal: bool) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => terminal && environment_allows_color(),
    }
}

/// `NO_COLOR` and `TERM=dumb` are requests to stop coloring; `CLICOLOR_FORCE`
/// overrides both. clap applies the same rules to its own rendering.
fn environment_allows_color() -> bool {
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| value != "0") {
        return true;
    }
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_explicit_choice_ignores_the_terminal() {
        assert!(colored(ColorChoice::Always, false));
        assert!(colored(ColorChoice::Always, true));
        assert!(!colored(ColorChoice::Never, true));
        assert!(!colored(ColorChoice::Never, false));
    }
    #[test]
    fn automatic_color_needs_a_terminal() {
        assert!(!colored(ColorChoice::Auto, false));
    }
}
