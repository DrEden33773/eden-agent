//! Frozen cross-session quotations: source history never becomes executable target work.
use crate::{
    Fault,
    coding::{Block, Item},
    context_edit::Entry,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Identifies the exact source branch independently of its later navigation or deletion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Source {
    pub session_id: u64,
    pub label: String,
    pub path: String,
    pub branch: String,
    pub head: Option<u64>,
}
/// Damaged files remain discoverable, but cannot be selected as complete references.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CatalogEntry {
    pub source: Source,
    pub diagnostic: Option<String>,
}
/// A branch choice is a durable tree node, never a navigation operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Branch {
    pub name: String,
    pub head: u64,
}
/// Image inclusion requires both the effective entry and its block position.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ImageSelection {
    pub entry_id: String,
    pub block_index: usize,
}
/// `None` chooses every selectable exchange; an empty list deliberately chooses none.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Selection {
    pub entry_ids: Option<Vec<String>>,
    #[serde(default)]
    pub images: Vec<ImageSelection>,
}
/// The effective projection is captured once so later source changes cannot alter insertion.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Preview {
    pub source: Source,
    pub entries: Vec<Entry>,
    pub source_system: Option<Vec<Item>>,
    pub projection_version: u32,
    pub estimated_tokens: u64,
}
/// Owned quotation material persists with drafts, queue entries and user messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Reference {
    pub id: String,
    pub source: Source,
    pub selected_entry_ids: Vec<String>,
    pub content: Vec<Block>,
    pub source_system: Option<Vec<Item>>,
    pub included_images: Vec<ImageSelection>,
    pub projection_version: u32,
}
/// Unknown source instructions cannot safely be inferred from current resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum SystemComparison {
    Same,
    Different,
    Unknown,
}
fn invalid(message: &str) -> Fault {
    Fault::new("InvalidReference", "session-reference", message)
}
/// Only conversation exchanges are selectable; instruction and provider state stay separate.
pub fn selectable(item: &Item) -> bool {
    matches!(item, Item::Message {role, ..} if role != "system" && role != "developer")
        || matches!(item, Item::ToolCall { .. } | Item::ToolResult { .. })
}
/// Extract the ordered final instructions for exact comparison at the request boundary.
pub fn system_items(items: &[Item]) -> Vec<Item> {
    items.iter().filter(|item| matches!(item, Item::Message {role, ..} if role == "system" || role == "developer")).cloned().collect()
}
impl Reference {
    /// Comparison uses actual provider-bound instructions, never a resource approximation.
    pub fn system_comparison(&self, actual_system: &[Item]) -> SystemComparison {
        match &self.source_system {
            None => SystemComparison::Unknown,
            Some(source) if source == actual_system => SystemComparison::Same,
            Some(_) => SystemComparison::Different,
        }
    }
}
impl Preview {
    /// Reject stale selections and preserve source order; no truncation or summarization occurs.
    pub fn freeze(&self, id: String, selection: &Selection) -> Result<Reference, Fault> {
        if id.trim().is_empty() {
            return Err(invalid("reference identity is empty"));
        }
        let available: BTreeSet<_> = self
            .entries
            .iter()
            .filter(|e| selectable(&e.item))
            .map(|e| e.id.as_str())
            .collect();
        let selected: BTreeSet<_> = selection.entry_ids.as_ref().map_or_else(
            || available.clone(),
            |ids| ids.iter().map(String::as_str).collect(),
        );
        if !selected.is_subset(&available)
            || selection
                .entry_ids
                .as_ref()
                .is_some_and(|ids| ids.len() != selected.len())
        {
            return Err(invalid("selection contains missing or duplicate entries"));
        }
        let images: BTreeSet<_> = selection.images.iter().collect();
        if images.len() != selection.images.len() {
            return Err(invalid("duplicate image selection"));
        }
        for image in &images {
            let entry = self
                .entries
                .iter()
                .find(|e| e.id == image.entry_id && selected.contains(e.id.as_str()))
                .ok_or_else(|| invalid("image entry is not selected"))?;
            if !matches!(
                blocks(&entry.item).get(image.block_index),
                Some(Block::Image { .. })
            ) {
                return Err(invalid("selected image is unavailable"));
            }
        }
        let mut content = Vec::new();
        let mut selected_entry_ids = Vec::new();
        for entry in self
            .entries
            .iter()
            .filter(|e| selected.contains(e.id.as_str()))
        {
            selected_entry_ids.push(entry.id.clone());
            quote(entry, &images, &mut content)?;
        }
        Ok(Reference {
            id,
            source: self.source.clone(),
            selected_entry_ids,
            content,
            source_system: self.source_system.clone(),
            included_images: selection.images.clone(),
            projection_version: self.projection_version,
        })
    }
}
fn blocks(item: &Item) -> &[Block] {
    match item {
        Item::Message { content, .. } => content,
        Item::ToolResult { result, .. } => &result.content,
        _ => &[],
    }
}
fn text(content: &mut Vec<Block>, value: String) {
    content.push(Block::Text { text: value });
}
fn quote(
    entry: &Entry,
    images: &BTreeSet<&ImageSelection>,
    content: &mut Vec<Block>,
) -> Result<(), Fault> {
    let label = match &entry.item {
        Item::Message { role, .. } => format!("Source {role} [{}]", entry.id),
        Item::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            text(
                content,
                format!(
                    "Source tool call [{}], {name} ({call_id}), quoted only:\n{arguments}",
                    entry.id
                ),
            );
            return Ok(());
        }
        Item::ToolResult { call_id, result } => {
            text(
                content,
                format!(
                    "Source tool result [{}] ({call_id}):\n{}",
                    entry.id, result.text
                ),
            );
            let metadata = serde_json::to_string(&(
                result.exit_code,
                result.truncated,
                &result.error,
                &result.details,
                &result.artifacts,
            ))
            .map_err(|e| invalid(&e.to_string()))?;
            format!("Source tool result metadata: {metadata}")
        }
        Item::ProviderState { .. } => return Ok(()),
    };
    text(content, label);
    for (block_index, block) in blocks(&entry.item).iter().enumerate() {
        match block {
            Block::Text { text: value } => text(content, value.clone()),
            Block::Image { .. }
                if images.contains(&ImageSelection {
                    entry_id: entry.id.clone(),
                    block_index,
                }) =>
            {
                content.push(block.clone())
            }
            Block::Image { .. } => text(
                content,
                "[Source image omitted; explicit selection required]".into(),
            ),
            Block::File { .. } => content.push(block.clone()),
        }
    }
    Ok(())
}
/// A transparent preview estimate; request-time model accounting remains authoritative.
pub fn estimate_blocks(blocks: &[Block]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            Block::Text { text } => text.chars().count() as u64 / 4 + 1,
            _ => 1024,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn preview() -> Preview {
        Preview {
            source: Source {
                session_id: 1,
                label: "source".into(),
                path: "saved".into(),
                branch: "main".into(),
                head: Some(1),
            },
            entries: vec![Entry {
                references: vec![],
                id: "e".into(),
                item: Item::Message {
                    role: "user".into(),
                    content: vec![
                        Block::Text {
                            text: "original".into(),
                        },
                        Block::Image {
                            media_type: "image/png".into(),
                            data: "bytes".into(),
                        },
                    ],
                },
            }],
            source_system: None,
            projection_version: 1,
            estimated_tokens: 0,
        }
    }
    #[test]
    fn freezing_keeps_owned_text_but_requires_explicit_images() {
        let mut preview = preview();
        let reference = preview.freeze("r".into(), &Selection::default()).unwrap();
        preview.entries.clear();
        assert!(reference.content.contains(&Block::Text {
            text: "original".into()
        }));
        assert!(
            !reference
                .content
                .iter()
                .any(|b| matches!(b, Block::Image { .. }))
        );
        assert_eq!(reference.system_comparison(&[]), SystemComparison::Unknown);
    }
    #[test]
    fn image_selection_requires_selected_entry_and_valid_block() {
        let selection = Selection {
            entry_ids: None,
            images: vec![ImageSelection {
                entry_id: "e".into(),
                block_index: 1,
            }],
        };
        assert!(
            preview()
                .freeze("r".into(), &selection)
                .unwrap()
                .content
                .iter()
                .any(|b| matches!(b, Block::Image { .. }))
        );
        assert!(
            preview()
                .freeze(
                    "r".into(),
                    &Selection {
                        entry_ids: Some(vec![]),
                        ..selection
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn source_tools_are_quoted_and_provider_state_is_never_selected() {
        let mut source = preview();
        source.entries.extend([
            Entry {
                references: vec![],
                id: "call".into(),
                item: Item::ToolCall {
                    call_id: "c".into(),
                    name: "shell".into(),
                    arguments: "{\"command\":\"danger\"}".into(),
                },
            },
            Entry {
                references: vec![],
                id: "thinking".into(),
                item: Item::ProviderState {
                    provider: "p".into(),
                    value: serde_json::json!({ "secret": "thinking" }),
                },
            },
            Entry {
                references: vec![],
                id: "system".into(),
                item: Item::Message {
                    role: "system".into(),
                    content: vec![Block::Text {
                        text: "source instruction".into(),
                    }],
                },
            },
        ]);
        let frozen = source.freeze("r".into(), &Selection::default()).unwrap();
        let text = serde_json::to_string(&frozen.content).unwrap();
        assert!(text.contains("danger"));
        assert!(text.contains("quoted only"));
        assert!(!text.contains("thinking"));
        assert!(!text.contains("source instruction"));
        assert_eq!(frozen.selected_entry_ids, vec!["e", "call"]);
    }
    #[test]
    fn missing_selections_are_refused_instead_of_silently_dropped() {
        assert!(
            preview()
                .freeze(
                    "r".into(),
                    &Selection {
                        entry_ids: Some(vec!["missing".into()]),
                        images: vec![]
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn selected_file_attachments_keep_their_captured_bytes() {
        let mut source = preview();
        let file = Block::File {
            name: "report.pdf".into(),
            media_type: "application/pdf".into(),
            data: "cGRmLWZpeHR1cmU=".into(),
        };
        if let Item::Message { content, .. } = &mut source.entries[0].item {
            content.push(file.clone());
        }
        let frozen = source
            .freeze("file-reference".into(), &Selection::default())
            .unwrap();
        assert!(frozen.content.contains(&file));
    }
}
