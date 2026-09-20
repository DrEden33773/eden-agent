//! Opt-in Eden-owned access learning. Search results alone never write history.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
struct Access {
    path: PathBuf,
    query: Option<String>,
    time: u64,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn file(directory: &Path, cwd: &Path) -> PathBuf {
    directory.join(format!(
        "{:x}.jsonl",
        Sha256::digest(cwd.to_string_lossy().as_bytes())
    ))
}
fn lock_error(error: std::fs::TryLockError) -> String {
    match error {
        std::fs::TryLockError::WouldBlock => "search history busy: another lock is held".into(),
        std::fs::TryLockError::Error(error) => format!("search history lock failed: {error}"),
    }
}
pub(crate) fn record(
    directory: &Path,
    cwd: &Path,
    path: &Path,
    query: Option<String>,
) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        // Windows cannot lock an append-only handle; retain append semantics with read access.
        .read(true)
        .append(true)
        .open(file(directory, cwd))
        .map_err(|e| e.to_string())?;
    output.try_lock().map_err(lock_error)?;
    let access = Access {
        path: path.into(),
        query,
        time: now(),
    };
    serde_json::to_writer(&mut output, &access).map_err(|e| e.to_string())?;
    writeln!(output)
        .and_then(|_| output.sync_data())
        .map_err(|e| e.to_string())
}
pub(crate) fn scores(
    directory: &Path,
    cwd: &Path,
    query: &str,
) -> Result<BTreeMap<PathBuf, f64>, String> {
    use std::io::BufRead;
    let input = match std::fs::File::open(file(directory, cwd)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    input.try_lock_shared().map_err(lock_error)?;
    let mut scores = BTreeMap::new();
    let time = now();
    for line in std::io::BufReader::new(&input).lines() {
        let access: Access =
            serde_json::from_str(&line.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let age = time.saturating_sub(access.time) as f64 / 86400.0;
        let weight = 2.0f64.powf(-age / 7.0)
            * (if access.query.as_deref() == Some(query) {
                2.0
            } else {
                1.0
            });
        *scores.entry(access.path).or_default() += weight;
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_lock_excludes_readers_and_writers_then_releases_for_append() {
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Directory(std::env::temp_dir().join(format!(
            "eden-search-history-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        std::fs::create_dir(&directory.0).unwrap();
        let cwd = std::fs::canonicalize(&directory.0).unwrap();
        let first = cwd.join("first.txt");
        let second = cwd.join("second.txt");
        record(&directory.0, &cwd, &first, None).unwrap();
        let history_path = file(&directory.0, &cwd);
        let before = std::fs::read(&history_path).unwrap();
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&history_path)
            .unwrap();
        owner.try_lock().unwrap();
        assert!(
            record(&directory.0, &cwd, &second, Some("needle".into()))
                .unwrap_err()
                .starts_with("search history busy:")
        );
        assert!(
            scores(&directory.0, &cwd, "needle")
                .unwrap_err()
                .starts_with("search history busy:")
        );
        drop(owner);
        assert_eq!(std::fs::read(&history_path).unwrap(), before);
        record(&directory.0, &cwd, &second, Some("needle".into())).unwrap();
        let after = std::fs::read_to_string(&history_path).unwrap();
        assert!(after.as_bytes().starts_with(&before));
        assert_eq!(after.lines().count(), 2);
        let ranked = scores(&directory.0, &cwd, "needle").unwrap();
        assert_eq!(ranked.len(), 2);
        assert!(ranked[&second] > ranked[&first]);
    }
}
