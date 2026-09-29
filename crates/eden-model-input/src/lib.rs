//! Shared model input rules: frontends make choices, while this backend validates and
//! prepares immutable image versions and resolves model-specific compaction budgets.
mod budget;
mod images;

pub use budget::*;
pub use images::*;
