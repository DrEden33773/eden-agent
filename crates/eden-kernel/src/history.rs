//! Independent public record reader. Does not load native libraries or execute recovered work.
use eden_protocol::{
    Fault,
    coding::{Record, decode_records},
};
use std::path::Path;
pub fn read(path: &Path) -> Result<Vec<Record>, Fault> {
    let bytes = std::fs::read(path)
        .map_err(|e| Fault::new("PersistenceFailure", "public-history", e.to_string()))?;
    decode_records(&bytes)
}
