//! Original message images are decoded once off-thread and contribute explicit transcript rows.
use crate::{
    image_preview::{self, Decoded},
    model::{Anchor, Message, Row},
};
use eden_protocol::coding::Block;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, mpsc},
};
type Key = (u64, usize);
#[derive(Clone)]
pub struct Slice {
    pub image: Arc<Decoded>,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}
impl std::fmt::Debug for Slice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSlice")
            .field("y", &self.y)
            .finish_non_exhaustive()
    }
}
struct Job {
    key: Key,
    image: Arc<Block>,
}
struct Reply {
    key: Key,
    result: Result<Arc<Decoded>, String>,
}
#[derive(Default)]
pub struct Thumbnails {
    known: BTreeSet<Key>,
    results: BTreeMap<Key, Result<Arc<Decoded>, String>>,
    revisions: BTreeMap<u64, u64>,
    worker: Option<(mpsc::Sender<Job>, mpsc::Receiver<Reply>)>,
}
impl Thumbnails {
    pub fn refresh(&mut self, messages: &[Message]) {
        let current: BTreeSet<_> = messages
            .iter()
            .flat_map(|m| (0..m.images.len()).map(move |index| (m.id, index)))
            .collect();
        self.results.retain(|key, _| current.contains(key));
        self.known.retain(|key| current.contains(key));
        for message in messages {
            for (index, image) in message.images.iter().enumerate() {
                let key = (message.id, index);
                if !self.known.insert(key) {
                    continue;
                }
                if self.worker.is_none() {
                    let (tx, rx) = mpsc::channel::<Job>();
                    let (reply_tx, reply_rx) = mpsc::channel();
                    tokio::task::spawn_blocking(move || {
                        while let Ok(job) = rx.recv() {
                            let result = image_preview::decode_thumbnail(&job.image).map(Arc::new);
                            if reply_tx
                                .send(Reply {
                                    key: job.key,
                                    result,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    self.worker = Some((tx, reply_rx));
                }
                if let Some((tx, _)) = &self.worker {
                    let _ = tx.send(Job {
                        key,
                        image: image.clone(),
                    });
                }
            }
        }
    }
    pub fn poll(&mut self) -> bool {
        let Some((_, rx)) = &self.worker else {
            return false;
        };
        let mut changed = false;
        while let Ok(reply) = rx.try_recv() {
            if self.known.contains(&reply.key) {
                *self.revisions.entry(reply.key.0).or_default() += 1;
                self.results.insert(reply.key, reply.result);
                changed = true;
            }
        }
        changed
    }
    pub fn revision(&self, record: u64) -> u64 {
        *self.revisions.get(&record).unwrap_or(&0)
    }
    pub fn append_rows(&self, message: &Message, width: usize, rows: &mut Vec<Row>) {
        let start = rows.iter().map(|r| r.anchor.line).max().unwrap_or(0) + 1;
        let mut next = start;
        for index in 0..message.images.len() {
            let result = self.results.get(&(message.id, index));
            let size = result.and_then(|r| r.as_ref().ok()).map(|image| {
                image_preview::fit(
                    image,
                    ratatui::layout::Rect::new(0, 0, width.min(60) as u16, 8),
                )
            });
            for y in 0..size.map_or(1, |size| size.height) {
                let image = result
                    .and_then(|r| r.as_ref().ok())
                    .zip(size)
                    .map(|(image, size)| Slice {
                        image: image.clone(),
                        y,
                        width: size.width,
                        height: size.height,
                    });
                let text = match result {
                    None => "[Original image preview loading]".into(),
                    Some(Err(error)) => format!("[Original image preview unavailable: {error}]"),
                    _ => String::new(),
                };
                rows.push(Row {
                    image,
                    tree_stem: false,
                    inset: 0,
                    summary: false,
                    source: None,
                    anchor: Anchor {
                        record: message.id,
                        line: next,
                        byte: 0,
                    },
                    text,
                    spans: vec![],
                    kind: message.role,
                    dim: false,
                    failed: false,
                });
                next += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};
    #[tokio::test]
    async fn original_images_decode_once_and_reserve_scrollable_transcript_rows() {
        let image = crate::image_preview::tests::block();
        let records: Vec<eden_protocol::coding::Record> =
            serde_json::from_value(serde_json::json!([{
                "schema_version": 1,
                "session_id": 7,
                "sequence": 1,
                "run_id": 1,
                "kind": "user_message",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "text", "text": "Original attachment" }, image],
                },
            }]))
            .unwrap();
        let mut app = crate::app::tests::app();
        app.messages = crate::projection::history(&records);
        assert_eq!(
            serde_json::to_value(&app.messages[0].images[0]).unwrap(),
            serde_json::to_value(&image).unwrap()
        );
        app.transcript_images.refresh(&app.messages);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !app.transcript_images.poll() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let revision = app.transcript_images.revision(1);
        let decoded = app.transcript_images.results[&(1, 0)]
            .as_ref()
            .unwrap()
            .clone();
        app.transcript_images.refresh(&app.messages);
        assert_eq!(app.transcript_images.revision(1), revision);
        assert!(Arc::ptr_eq(
            &decoded,
            app.transcript_images.results[&(1, 0)].as_ref().unwrap()
        ));
        let wide = Rect::new(0, 0, 100, 32);
        let mut buffer = Buffer::empty(wide);
        crate::view::render(&mut app, &mut buffer, wide, false);
        let wide_rows = app.rows.iter().filter(|row| row.image.is_some()).count();
        assert!(wide_rows > 1);
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "▀"));
        let image_row = app
            .rows
            .iter()
            .find(|row| row.image.is_some())
            .unwrap()
            .anchor;
        app.anchor = image_row;
        app.follow = false;
        let narrow = Rect::new(0, 0, 20, 14);
        crate::view::render(&mut app, &mut Buffer::empty(narrow), narrow, true);
        assert!(app.rows.iter().filter(|row| row.image.is_some()).count() < wide_rows);
        assert!(app.top < app.rows.len());
        assert_eq!(app.document_height, app.rows.len());
    }
    #[test]
    fn a_late_thumbnail_for_a_removed_record_does_not_return_to_the_transcript() {
        let mut thumbnails = Thumbnails::default();
        let (tx, _) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        thumbnails.worker = Some((tx, rx));
        reply_tx
            .send(Reply {
                key: (99, 0),
                result: Ok(Arc::new(
                    image_preview::decode_thumbnail(&crate::image_preview::tests::block()).unwrap(),
                )),
            })
            .unwrap();
        assert!(!thumbnails.poll());
        assert!(thumbnails.results.is_empty());
    }
}
