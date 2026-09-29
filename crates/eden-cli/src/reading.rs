//! Independent reading never loads saved native libraries or repairs source bytes.
use super::*;
pub(super) struct Document {
    pub(super) history: Vec<eden_protocol::coding::Record>,
    pub(super) reading: Option<eden_protocol::delivery::ReadingDocument>,
    pub(super) diagnostic: Option<String>,
}
pub(super) fn read_document(path: &Path) -> Result<Document, Fault> {
    let text = std::fs::read_to_string(path).map_err(|e| fault("FileFailure", e.to_string()))?;
    let first = text
        .lines()
        .next()
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    if let Some(format) = first.as_ref().and_then(|v| v["format"].as_str()) {
        let mut reading = eden_protocol::delivery::ReadingDocument {
            format: format.into(),
            ..Default::default()
        };
        if format != "eden-reading-v1" {
            reading.diagnostic = Some(format!("Unknown reading format {format}; source fallback"));
        }
        for (index, line) in text.lines().enumerate().skip(1) {
            match serde_json::from_str::<Value>(line) {
                Ok(value) => reading.entries.push(value),
                Err(error) => {
                    reading.diagnostic =
                        Some(format!("Unreadable tail at line {}: {error}", index + 1));
                    break;
                }
            }
        }
        return Ok(Document {
            history: vec![],
            reading: Some(reading),
            diagnostic: None,
        });
    }
    let scan = eden_protocol::history::scan_records(text.as_bytes());
    let history = match eden_protocol::history::active_path(&scan.records) {
        Ok(history) => history,
        Err(error) => {
            return Ok(Document {
                history: vec![],
                reading: Some(eden_protocol::delivery::ReadingDocument {
                    format: "history-source".into(),
                    entries: text
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect(),
                    diagnostic: Some(error.to_string()),
                }),
                diagnostic: scan.diagnostic,
            });
        }
    };
    Ok(Document {
        history,
        reading: None,
        diagnostic: scan.diagnostic,
    })
}
