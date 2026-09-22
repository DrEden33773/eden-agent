//! Portable reading artifacts and explicit publication, separate from restorable history.
use crate::coding::Record;
use serde::{Deserialize, Serialize};
/// Independent renderer selected through the ordinary native service registry.
pub const EXPORTER: &str = "eden.exporter.v1";
/// Explicit remote publisher; preparation itself never calls this role.
pub const SHARE_TARGET: &str = "eden.share-target.v1";
/// Filtering occurs before rendering; an empty run list means all selected-path turns.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(missing_docs)]
pub struct Selection {
    pub head: Option<u64>,
    pub runs: Vec<u64>,
    pub messages: bool,
    pub tools: bool,
    pub thinking: bool,
    pub attachments: bool,
    pub full_outputs: bool,
    pub extensions: bool,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            head: None,
            runs: vec![],
            messages: true,
            tools: true,
            thinking: false,
            attachments: false,
            full_outputs: false,
            extensions: true,
        }
    }
}
/// Reading JSONL is deliberately not the public-history transaction format.
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Format {
    #[default]
    Jsonl,
}
impl<'de> Deserialize<'de> for Format {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "jsonl" => Ok(Self::Jsonl),
            "html" => Err(serde::de::Error::custom(
                "HTML export has been removed; use jsonl",
            )),
            _ => Err(serde::de::Error::unknown_variant(&value, &["jsonl"])),
        }
    }
}
/// A committed snapshot plus an explicit selection, never live event deltas.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ExportRequest {
    pub records: Vec<Record>,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default)]
    pub format: Format,
}
/// Exact bytes for preview and publication. Warnings describe omitted unavailable content.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Artifact {
    pub media_type: String,
    pub filename: String,
    pub content: String,
    pub warnings: Vec<String>,
}
/// A publisher receives already prepared bytes, never an instruction to reread history.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct PublishRequest {
    pub artifact: Artifact,
    pub confirmed: bool,
}
/// A secret gist is accessible to anyone with its URL; it is not access-controlled storage.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct PublishReply {
    pub url: String,
    pub visibility: String,
    pub notice: String,
}

#[cfg(test)]
mod tests {
    use super::Format;

    #[test]
    fn reading_format_defaults_to_jsonl_and_rejects_html() {
        assert_eq!(
            serde_json::to_string(&Format::default()).unwrap(),
            "\"jsonl\""
        );
        assert!(matches!(
            serde_json::from_str::<Format>("\"jsonl\""),
            Ok(Format::Jsonl)
        ));
        assert!(
            serde_json::from_str::<Format>("\"html\"")
                .unwrap_err()
                .to_string()
                .contains("HTML export has been removed")
        );
    }
}
