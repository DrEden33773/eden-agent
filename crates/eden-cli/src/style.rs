//! Terminal palette shared by clap's rendering and by runtime messages.
//!
//! One set of constants keeps a rendered usage error and a runtime failure
//! looking like they came from the same program.
use clap::builder::styling::{AnsiColor, Effects, Style, Styles};

/// Section headings such as `Usage:` and `Commands:`.
pub const HEADER: Style = AnsiColor::BrightGreen.on_default().effects(Effects::BOLD);
/// The usage grammar line.
pub const USAGE: Style = HEADER;
/// Literal option, value and command spellings.
pub const LITERAL: Style = AnsiColor::BrightCyan.on_default().effects(Effects::BOLD);
/// Placeholder names inside angle brackets.
pub const PLACEHOLDER: Style = AnsiColor::Cyan.on_default();
/// Failures.
pub const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
/// Something was skipped or needs attention.
pub const WARN: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
/// A remark that is not a problem.
pub const NOTE: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
/// Input clap accepted.
pub const VALID: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
/// Input clap rejected.
pub const INVALID: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);

/// The palette clap renders help and usage errors with.
pub fn styles() -> Styles {
    Styles::styled()
        .header(HEADER)
        .usage(USAGE)
        .literal(LITERAL)
        .placeholder(PLACEHOLDER)
        .error(ERROR)
        .valid(VALID)
        .invalid(INVALID)
}
