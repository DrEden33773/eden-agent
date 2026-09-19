//! Formatting for the two macro DSLs that rustfmt cannot reach.
//!
//! rustfmt leaves a macro body alone when it cannot parse it as Rust, which is
//! exactly what `json!`'s `"key": value` object syntax is. This crate lowers such
//! a body into an equivalent Rust expression that rustfmt does parse, hands the
//! whole file to rustfmt, and then lifts the formatted expression back into the
//! DSL it came from.
//! Every layout decision stays with rustfmt: lowering only renames syntax.
//!
//! The pipeline is `lower -> rustfmt -> lift`, and both directions work on byte
//! ranges of the text being processed:
//!
//! - lower: walk the source tokens, rebuild each target macro body from the
//!   byte ranges of its parts, and record the original delimiter in the
//!   sentinel's suffix so lift can restore it.
//! - lift: re-parse the formatted text, read every sentinel match with `syn`,
//!   and place the values it finds back into the DSL by construction position.
//!
//! Nothing is ever reconstructed from `TokenStream::to_string()`, so comments,
//! blank lines and raw string contents survive both directions.

pub mod edit;
pub mod engine;
pub mod json;
pub mod lint;
pub mod scan;
pub mod select;

pub use edit::LineIndex;
pub use engine::{Error, Options, format_source};
pub use lint::Violation;
