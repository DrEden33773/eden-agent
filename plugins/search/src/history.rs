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
pub fn record(
    directory: &Path,
    cwd: &Path,
    path: &Path,
    query: Option<String>,
) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file(directory, cwd))
        .map_err(|e| e.to_string())?;
    output
        .try_lock()
        .map_err(|e| format!("search history busy: {e}"))?;
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
pub fn scores(directory: &Path, cwd: &Path, query: &str) -> Result<BTreeMap<PathBuf, f64>, String> {
    use std::io::BufRead;
    let input = match std::fs::File::open(file(directory, cwd)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    input
        .try_lock_shared()
        .map_err(|e| format!("search history busy: {e}"))?;
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
