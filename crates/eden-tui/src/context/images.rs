//! Image choices retain their captured revision independently of the document draft.
use super::*;
use crate::image_preview::{self, Decoded};
use eden_protocol::context_edit::{ImageAction, ImageEdit, Revision};
use std::{
    collections::BTreeMap,
    sync::{Arc, mpsc},
};
type Key = (String, usize);
struct ImageItem {
    original: Block,
    sent: Option<Block>,
    metadata: String,
}
struct Job {
    generation: u64,
    key: (Key, bool, u64),
    block: Block,
}
struct Reply {
    generation: u64,
    key: (Key, bool, u64),
    result: Result<Arc<Decoded>, String>,
}
#[derive(Default)]
pub(super) struct Images {
    items: BTreeMap<Key, ImageItem>,
    selected: usize,
    original: bool,
    revision: Option<Revision>,
    choices: BTreeMap<Key, ImageAction>,
    pub pending: Option<Value>,
    reviewed: bool,
    generation: u64,
    version: u64,
    seen: u64,
    worker: Option<(mpsc::Sender<Job>, mpsc::Receiver<Reply>)>,
    cache: BTreeMap<(Key, bool, u64), Arc<Decoded>>,
    loaded: Option<Arc<Decoded>>,
    status: String,
}
impl Images {
    pub fn history(&mut self, records: &[eden_protocol::coding::Record]) -> bool {
        let before = self.version;
        for record in records
            .iter()
            .filter(|r| r.sequence > self.seen && r.kind == "image_version")
        {
            let payload = &record.payload;
            let (Some(entry), Some(index)) = (
                payload["entry_id"].as_str(),
                payload["block_index"].as_u64(),
            ) else {
                continue;
            };
            let image = &payload["image"];
            let Some(original) = image["original"]
                .as_u64()
                .and_then(|i| image["payloads"].get(i as usize))
                .and_then(|v| serde_json::from_value::<Block>(v.clone()).ok())
            else {
                continue;
            };
            let active = image
                .get("active")
                .or_else(|| image.get("current"))
                .and_then(Value::as_u64);
            let sent = active
                .and_then(|i| image["versions"].get(i as usize))
                .and_then(|v| v["payload"].as_u64())
                .and_then(|i| image["payloads"].get(i as usize))
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            self.items.insert(
                (entry.to_owned(), index as usize),
                ImageItem {
                    original,
                    sent,
                    metadata: format!(
                        "Image version record #{} · omitted: {}\n{}",
                        record.sequence,
                        image["omitted"].as_bool().unwrap_or(false),
                        pretty(&image["versions"])
                    ),
                },
            );
            self.version = record.sequence;
            self.loaded = None;
        }
        self.seen = records.last().map_or(self.seen, |r| r.sequence);
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.version != before
    }
    pub fn reset_choices(&mut self) {
        self.choices.clear();
        self.reviewed = false;
    }
    pub fn capture(&mut self, snapshot: &Snapshot) {
        self.revision = Some(snapshot.revision.clone());
        self.items.retain(|(id, _), _| {
            snapshot
                .original
                .entries
                .iter()
                .any(|entry| &entry.id == id)
        });
        for entry in &snapshot.original.entries {
            let blocks = match &entry.item {
                Item::Message { content, .. } => content,
                Item::ToolResult { result, .. } => &result.content,
                _ => continue,
            };
            for (index, block) in blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| matches!(b, Block::Image { .. }))
            {
                self.items
                    .entry((entry.id.clone(), index))
                    .or_insert_with(|| ImageItem {
                        original: block.clone(),
                        sent: None,
                        metadata: "Original captured · no image version record yet".into(),
                    });
            }
        }
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.reviewed = false;
    }
    pub(super) fn load(&mut self) {
        self.generation += 1;
        self.loaded = None;
        let Some((key, item)) = self.items.iter().nth(self.selected) else {
            self.status = "No images in this context".into();
            return;
        };
        let key = (key.clone(), self.original, self.version);
        let block = if self.original {
            Some(&item.original)
        } else {
            item.sent.as_ref()
        };
        let Some(block) = block else {
            self.status = "No active sent version · original is retained; o previews it".into();
            return;
        };
        if let Some(image) = self.cache.get(&key) {
            self.loaded = Some(image.clone());
            self.status.clear();
            return;
        }
        if self.worker.is_none() {
            let (tx, rx) = mpsc::channel::<Job>();
            let (result_tx, result_rx) = mpsc::channel();
            tokio::task::spawn_blocking(move || {
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    let result = image_preview::decode(&job.block).map(Arc::new);
                    if result_tx
                        .send(Reply {
                            generation: job.generation,
                            key: job.key,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            self.worker = Some((tx, result_rx));
        }
        self.status = "Decoding image preview…".into();
        if let Some((tx, _)) = &self.worker {
            let _ = tx.send(Job {
                generation: self.generation,
                key,
                block: block.clone(),
            });
        }
    }
    pub fn poll(&mut self) -> bool {
        let Some((_, rx)) = &self.worker else {
            return false;
        };
        let mut changed = false;
        while let Ok(reply) = rx.try_recv() {
            if reply.generation != self.generation {
                continue;
            }
            changed = true;
            match reply.result {
                Ok(image) => {
                    if self.cache.len() >= 8 {
                        self.cache.clear();
                    }
                    self.cache.insert(reply.key, image.clone());
                    self.loaded = Some(image);
                    self.status.clear();
                }
                Err(error) => {
                    self.status = format!("Preview unavailable · original retained: {error}")
                }
            }
        }
        changed
    }
}
impl App {
    pub(crate) fn context_images_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                self.context.images.selected = self.context.images.selected.saturating_sub(1);
                self.context.images.load();
            }
            KeyCode::Down => {
                self.context.images.selected = (self.context.images.selected + 1)
                    .min(self.context.images.items.len().saturating_sub(1));
                self.context.images.load();
            }
            KeyCode::Char('o') => {
                self.context.images.original = !self.context.images.original;
                self.context.images.load();
            }
            KeyCode::Char('1' | '2' | '3') if !self.read_only => {
                if let Some(key_image) = self
                    .context
                    .images
                    .items
                    .keys()
                    .nth(self.context.images.selected)
                    .cloned()
                {
                    self.context.images.choices.insert(
                        key_image,
                        match key.code {
                            KeyCode::Char('1') => ImageAction::Preserve,
                            KeyCode::Char('2') => ImageAction::Omit,
                            _ => ImageAction::ReAdapt,
                        },
                    );
                    self.context.images.reviewed = false;
                    self.context.status =
                        "Image choice staged · p reviews all choices; a applies".into();
                }
            }
            KeyCode::Char('p') => {
                self.context.images.reviewed = true;
                self.context.status = format!(
                    "Review image choices: {} · a applies at the captured revision",
                    self.context
                        .images
                        .choices
                        .iter()
                        .map(|((entry, block), action)| format!("{entry}[{block}] {action:?}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            KeyCode::Char('a')
                if !self.read_only
                    && self.context.images.reviewed
                    && !self.context.images.choices.is_empty() =>
            {
                let Some(revision) = self.context.images.revision.clone() else {
                    return;
                };
                let mut choices: BTreeMap<String, BTreeMap<usize, ImageAction>> = BTreeMap::new();
                for ((entry, block), action) in &self.context.images.choices {
                    choices
                        .entry(entry.clone())
                        .or_default()
                        .insert(*block, *action);
                }
                let body = json!({
                    "request_id": self.id(),
                    "edit": ImageEdit { revision, choices },
                });
                self.context.images.pending = Some(body.clone());
                self.context.waiting = true;
                self.context.status = "Applying reviewed image choices…".into();
                self.dispatch("/context/images", body);
            }
            _ => {}
        }
    }
    pub(crate) fn context_images_reply(&mut self, body: &Value, result: Result<Value, Fault>) {
        if !self
            .context
            .images
            .pending
            .as_ref()
            .is_some_and(|p| p["request_id"] == body["request_id"])
        {
            return;
        }
        self.context.waiting = false;
        match result {
            Ok(value) => match serde_json::from_value::<Snapshot>(value) {
                Ok(snapshot) => {
                    self.context.images.pending = None;
                    self.context.images.choices.clear();
                    self.context.images.capture(&snapshot);
                    self.context.refreshed = Some(snapshot);
                    self.context.display_cache = None;
                    self.context.status = "Image choices applied · document draft retained; \
                                           inspect versions or explicitly reload/rebase"
                        .into();
                }
                Err(error) => {
                    self.context.status =
                        format!("Image receipt unreadable · t retries same transaction: {error}")
                }
            },
            Err(error) => {
                if error.source != "live-client" {
                    self.context.images.pending = None;
                }
                self.context.status =
                    format!("Image choices retained · b rebases / r reloads / t retries: {error}");
            }
        }
    }
}
pub(super) fn render(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    mono: bool,
) -> Option<crate::image_preview::Placement> {
    let images = &app.context.images;
    let Some(((entry, block), item)) = images.items.iter().nth(images.selected) else {
        Paragraph::new("No images in captured context")
            .style(p.style())
            .render(area, buf);
        return None;
    };
    let summary = format!(
        "Image {}/{} · {}[{}] · {} · choice: {:?}\n{}\n{}",
        images.selected + 1,
        images.items.len(),
        entry,
        block,
        if images.original {
            "Original"
        } else {
            "Active sent version"
        },
        images.choices.get(&(entry.clone(), *block)),
        images.status,
        format_args!(
            "{}{}",
            images
                .loaded
                .as_ref()
                .map(|i| format!("Decoded {}×{} pixels · ", i.width, i.height))
                .unwrap_or_default(),
            item.metadata
        )
    );
    let lines = area.height.min(5);
    Paragraph::new(summary)
        .style(p.style())
        .wrap(Wrap { trim: false })
        .render(Rect::new(area.x, area.y, area.width, lines), buf);
    let raster_area = Rect::new(
        area.x,
        area.y + lines,
        area.width,
        area.height.saturating_sub(lines),
    );
    if let Some(image) = &images.loaded {
        image_preview::raster(image, buf, raster_area, mono);
        if image_preview::kitty_available() && !mono {
            return Some(crate::image_preview::Placement {
                image: image.clone(),
                area: image_preview::fit(image, raster_area),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Snapshot {
        serde_json::from_value(json!({
            "revision": { "session_id": 7, "sequence": 3, "head": 3, "branch": "main" },
            "original": {
                "entries": [{
                    "id": "one",
                    "item": {
                        "type": "message",
                        "role": "user",
                        "content": [crate::image_preview::tests::block()],
                    },
                }],
                "tools": [],
            },
            "effective": { "entries": [], "tools": [] },
            "edits": [],
            "last_request": null,
        }))
        .unwrap()
    }
    #[test]
    fn original_and_sent_previews_follow_retained_version_metadata() {
        let mut images = Images::default();
        let original = crate::image_preview::tests::block();
        let records = vec![eden_protocol::coding::Record {
            parent_id: None,
            branch: "main".into(),
            schema_version: 1,
            session_id: 7,
            sequence: 3,
            run_id: 0,
            kind: "image_version".into(),
            payload: json!({
                "entry_id": "one",
                "block_index": 0,
                "image": {
                    "payloads": [original],
                    "original": 0,
                    "versions": [{
                        "payload": 0,
                        "provider": "p",
                        "model": "m",
                        "width": 4,
                        "height": 2,
                        "explanation": "preserved",
                    }],
                    "active": 0,
                    "omitted": false,
                },
            }),
        }];
        assert!(images.history(&records));
        assert!(!images.history(&records));
        images.capture(&snapshot());
        let item = images.items.get(&("one".into(), 0)).unwrap();
        assert_eq!(pretty(&item.original), pretty(item.sent.as_ref().unwrap()));
        assert!(item.metadata.contains("preserved"));
    }
    #[tokio::test]
    async fn image_conflict_preserves_choices_and_document_draft() {
        let mut app = crate::app::tests::app();
        app.context.capture(snapshot());
        app.context.view = 7;
        app.context_images_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        app.context_images_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        app.context_images_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let body = app.context.images.pending.clone().unwrap();
        assert_eq!(body["edit"]["choices"]["one"]["0"], "omit");
        app.context_images_reply(
            &body,
            Err(Fault::new("ContextConflict", "context", "stale")),
        );
        assert!(app.context.images.pending.is_none());
        assert_eq!(app.context.images.choices.len(), 1);
        assert_eq!(app.context.snapshot.as_ref().unwrap().revision.sequence, 3);
        assert!(app.context.draft.as_ref().unwrap().entries.is_empty());
    }
    #[test]
    fn stale_decode_cannot_replace_the_selected_image() {
        let mut images = Images::default();
        let (job_tx, _job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        images.worker = Some((job_tx, reply_rx));
        images.generation = 2;
        reply_tx
            .send(Reply {
                generation: 1,
                key: (("old".into(), 0), true, 1),
                result: Ok(Arc::new(
                    image_preview::decode(&crate::image_preview::tests::block()).unwrap(),
                )),
            })
            .unwrap();
        assert!(!images.poll());
        assert!(images.loaded.is_none());
    }
}
