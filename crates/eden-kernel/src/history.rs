//! Independent public record reader. Does not load native libraries or execute recovered work.
use eden_protocol::{
    Fault,
    coding::{Record, decode_records},
};
use std::path::Path;
/// Read every record of a history file, refusing one whose tail is damaged so a
/// caller cannot mistake a truncated read for a complete history.
pub fn read(path: &Path) -> Result<Vec<Record>, Fault> {
    let bytes = std::fs::read(path)
        .map_err(|e| Fault::new("PersistenceFailure", "public-history", e.to_string()))?;
    decode_records(&bytes)
}

/// Inspect the readable committed prefix; damage is reported without modifying it.
pub fn inspect(path: &Path) -> Result<eden_protocol::history::HistoryScan, Fault> {
    let bytes = std::fs::read(path)
        .map_err(|error| Fault::new("PersistenceFailure", "public-history", error.to_string()))?;
    Ok(eden_protocol::history::scan_records(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspect_exposes_prefix_without_repairing_damaged_tail() {
        let path = std::env::temp_dir().join(format!("eden-inspect-{}.jsonl", std::process::id()));
        let text = concat!(
            "{\"schema_version\":1,\"session_id\":7,\"sequence\":1,\"run_id\":1,\"kind\":\"user\",\
             \"payload\":{}}\n",
            "{\"partial\":",
        );
        let bytes = text.as_bytes();
        std::fs::write(&path, bytes).unwrap();
        let scan = inspect(&path).unwrap();
        assert_eq!(scan.records.len(), 1);
        assert!(scan.diagnostic.unwrap().contains("line 2"));
        assert!(read(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }
}
