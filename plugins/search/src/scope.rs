//! Scope revision uses the same ignore policy as FFF 0.10.6's Rust walker.
//! The exclusion list below is adapted from fff-search/src/ignore.rs at
//! c6013ba6a5918221b6c482486aca01acc0830825, MIT; see third-party/fff-0.10.6-LICENSE.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};
pub type Stamp = BTreeMap<PathBuf, (PathBuf, u64, Option<SystemTime>)>;
pub fn stamp(path: &Path, follow: bool, excluded: &[PathBuf]) -> Result<Stamp, String> {
    let mut output = BTreeMap::new();
    let is_git = path.ancestors().any(|parent| parent.join(".git").exists());
    let mut builder = ignore::WalkBuilder::new(path);
    builder
        .hidden(!is_git)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .follow_links(follow);
    if !is_git && path.is_dir() {
        let mut excludes = ignore::overrides::OverrideBuilder::new(path);
        for dir in IGNORED_DIRS {
            excludes
                .add(&format!("!**/{dir}/"))
                .map_err(|e| e.to_string())?;
        }
        builder.overrides(excludes.build().map_err(|e| e.to_string())?);
    }
    let excluded = excluded.to_vec();
    builder.filter_entry(move |entry| {
        entry.file_name() != ".git" && !is_excluded(entry.path(), &excluded)
    });
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if is_loop(&error) => continue,
            Err(error) => return Err(error.to_string()),
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let canonical = std::fs::canonicalize(entry.path()).map_err(|e| e.to_string())?;
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        output.insert(
            entry.path().to_owned(),
            (canonical, metadata.len(), metadata.modified().ok()),
        );
    }
    Ok(output)
}
/// Match lexical paths as well as aliases to excluded physical state.
pub fn is_excluded(path: &Path, excluded: &[PathBuf]) -> bool {
    let canonical = std::fs::canonicalize(path).ok();
    excluded.iter().any(|root| {
        path.starts_with(root)
            || canonical.as_ref().is_some_and(|p| {
                p.starts_with(std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
            })
    })
}
pub(crate) const IGNORED_DIRS: &[&str] = &[
    // various dev tools that can be meet in the developer app
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    "target/debug",
    "target/release",
    "target/rust-analyzer",
    "target/criterion",
    // Language package caches in non-git roots.
    "go/pkg/mod",
    ".cargo/registry",
    ".rustup/toolchains",
    ".gradle/caches",
    ".m2/repository",
    ".npm/_cacache",
    ".pub-cache",
    #[cfg(not(target_os = "windows"))]
    ".local/state",
    // this contains tons of logs which generate too much watcher noise
    #[cfg(target_os = "macos")]
    "Library/Application Support",
    #[cfg(target_os = "macos")]
    "Library/Caches",
    #[cfg(target_os = "macos")]
    "Library/Containers",
    // sandboxed apps data
    #[cfg(target_os = "macos")]
    "Library/Group Containers",
    // random application data and networking
    #[cfg(target_os = "macos")]
    "Library/pnpm",
    #[cfg(target_os = "macos")]
    "Library/Metadata",
    #[cfg(target_os = "macos")]
    "Library/Developer/CoreSimulator",
    #[cfg(target_os = "macos")]
    "Library/Android",
    #[cfg(target_os = "macos")]
    "Library/Logs",
    #[cfg(target_os = "macos")]
    "Library/Daemon Containers",
    #[cfg(target_os = "macos")]
    "Library/Trial",
    #[cfg(target_os = "macos")]
    "Library/Preferences",
    #[cfg(target_os = "macos")]
    "Library/Messages",
    #[cfg(target_os = "macos")]
    "Library/IdentityServices",
    #[cfg(target_os = "windows")]
    "bin/Debug",
    #[cfg(target_os = "windows")]
    "bin/Release",
    #[cfg(target_os = "windows")]
    "Program Files",
    #[cfg(target_os = "windows")]
    "Program Files (x86)",
    #[cfg(target_os = "windows")]
    "AppData/Local",
    #[cfg(target_os = "windows")]
    "AppData/Roaming",
];
fn is_loop(error: &ignore::Error) -> bool {
    match error {
        ignore::Error::Loop { .. } => true,
        ignore::Error::WithPath { err, .. }
        | ignore::Error::WithDepth { err, .. }
        | ignore::Error::WithLineNumber { err, .. } => is_loop(err),
        _ => false,
    }
}
