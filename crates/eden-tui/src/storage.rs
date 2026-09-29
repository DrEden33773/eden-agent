use crate::model::SavedDraft;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

pub struct DraftStore {
    tx: Option<mpsc::Sender<SavedDraft>>,
    worker: Option<thread::JoinHandle<()>>,
    path: PathBuf,
    errors: mpsc::Receiver<String>,
}
impl DraftStore {
    pub fn new(dir: &Path, frontend: &str) -> io::Result<Self> {
        if frontend.is_empty()
            || !frontend
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frontend ID must use letters, digits, - or _",
            ));
        }
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("draft-{frontend}.json"));
        let target = path.clone();
        let (tx, rx) = mpsc::channel::<SavedDraft>();
        let (failures, errors) = mpsc::channel();
        let worker = thread::spawn(move || {
            while let Ok(mut draft) = rx.recv() {
                while let Ok(latest) = rx.try_recv() {
                    draft = latest;
                }
                let result = (|| -> io::Result<()> {
                    use std::io::Write;
                    let bytes = serde_json::to_vec_pretty(&draft).map_err(io::Error::other)?;
                    let tmp = target.with_extension(format!("{}.tmp", std::process::id()));
                    let mut options = fs::OpenOptions::new();
                    options.write(true).create(true).truncate(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = options.open(&tmp)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    drop(file);
                    #[cfg(windows)]
                    if target.exists() {
                        fs::remove_file(&target)?;
                    }
                    fs::rename(tmp, &target)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = failures.send(format!("Draft could not be saved: {error}"));
                }
            }
        });
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            path,
            errors,
        })
    }
    pub fn error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
    pub fn load(&self) -> Option<SavedDraft> {
        serde_json::from_slice(&fs::read(&self.path).ok()?).ok()
    }
    pub fn save(&self, draft: SavedDraft) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(draft);
        }
    }
}
impl Drop for DraftStore {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, Draft};
    #[test]
    fn recovery_retains_attachment_bytes_and_frontend_identity() {
        let dir = std::env::temp_dir().join(format!("s4-store-test-{}", std::process::id()));
        let store = DraftStore::new(&dir, "terminal-a").unwrap();
        store.save(SavedDraft {
            sessions: std::collections::BTreeMap::from([(
                "other-session".into(),
                Draft {
                    text: "other draft".into(),
                    ..Draft::default()
                },
            )]),
            version: 1,
            pending: None,
            session: "task-a".into(),
            frontend: "terminal-a".into(),
            draft: Draft {
                references: vec![crate::references::tests::frozen().into()],
                text: "中文\n草稿".into(),
                cursor: 6,
                attachments: vec![Attachment {
                    name: "removed.txt".into(),
                    source: "/no-longer-exists".into(),
                    bytes: b"immutable".to_vec().into(),
                    media_type: None,
                    image: false,
                }],
            },
        });
        drop(store);
        let store = DraftStore::new(&dir, "terminal-a").unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.draft.attachments[0].bytes.as_ref(), b"immutable");
        assert_eq!(loaded.frontend, "terminal-a");
        assert_eq!(
            *loaded.draft.references[0],
            crate::references::tests::frozen()
        );
        assert_eq!(loaded.sessions["other-session"].text, "other draft");
        let other = DraftStore::new(&dir, "terminal-b").unwrap();
        assert!(other.load().is_none());
        drop(other);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
}
