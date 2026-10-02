//! Independent reading never loads saved native libraries or repairs source bytes.
use super::*;
pub(super) struct Document {
    pub(super) audit: Vec<u8>,
    pub(super) history: Vec<eden_protocol::coding::Record>,
    pub(super) reading: Option<eden_protocol::delivery::ReadingDocument>,
    pub(super) diagnostic: Option<String>,
}
pub(super) fn read_document(path: &Path) -> Result<Document, Fault> {
    let audit = std::fs::read(path).map_err(|e| fault("FileFailure", e.to_string()))?;
    let text = std::str::from_utf8(&audit).map_err(|e| fault("FileFailure", e.to_string()))?;
    #[derive(serde::Deserialize)]
    struct FormatHeader {
        format: Option<String>,
    }
    let first = text
        .lines()
        .next()
        .and_then(|line| serde_json::from_str::<FormatHeader>(line).ok());
    if let Some(format) = first.as_ref().and_then(|v| v.format.as_deref()) {
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
            audit: vec![],
            history: vec![],
            reading: Some(reading),
            diagnostic: None,
        });
    }
    let scan = eden_protocol::history_read::read_view(
        audit.as_slice(),
        eden_protocol::history_read::ReadView::Presentation,
        &|| false,
    )?
    .ok_or_else(|| fault("Cancelled", "history read cancelled"))?;
    if scan.records.is_empty() && scan.diagnostic.is_some() {
        return Ok(Document {
            audit: vec![],
            history: vec![],
            reading: Some(eden_protocol::delivery::ReadingDocument {
                format: "history-source".into(),
                entries: text
                    .lines()
                    .map(|line| {
                        serde_json::from_str(line).unwrap_or_else(|_| Value::String(line.into()))
                    })
                    .collect(),
                diagnostic: scan.diagnostic,
            }),
            diagnostic: None,
        });
    }
    let history = match eden_protocol::history::active_path(&scan.records) {
        Ok(history) => history,
        Err(error) => {
            return Ok(Document {
                audit: vec![],
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
        audit,
        history,
        reading: None,
        diagnostic: scan.diagnostic,
    })
}
