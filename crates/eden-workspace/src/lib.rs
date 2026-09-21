//! Data-only workspace bootstrap. No library, shell or credential command is executed here.
pub mod packages;
pub mod paths;
use eden_protocol::Fault;
use eden_protocol::resources::Diagnostic;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One recorded trust decision: a project root and whether it is trusted. The
/// nearest recorded ancestor decides, so a nested grant can override a denial.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field names and types state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct TrustEntry {
    pub root: PathBuf,
    pub trusted: bool,
}

/// What discovery is configured with: the global directory, an explicit trust
/// answer that outranks the saved one, and settings the caller overrides.
#[derive(Clone, Debug)]
// Field names and types state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct WorkspaceOptions {
    pub global_dir: PathBuf,
    pub project_trust: Option<bool>,
    pub overrides: Value,
}
impl Default for WorkspaceOptions {
    fn default() -> Self {
        let global_dir = std::env::var_os("EDEN_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                paths::user_home()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".eden/agent")
            });
        Self {
            global_dir,
            project_trust: None,
            overrides: serde_json::json!({}),
        }
    }
}

/// The resolved workspace a session runs in: its cwd, its global directory,
/// whether the project is trusted, the merged settings, and every diagnostic
/// raised while resolving them — including the one that says why untrusted
/// project resources were skipped.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field names and types state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub global_dir: PathBuf,
    pub trusted: bool,
    pub settings: Value,
    pub diagnostics: Vec<Diagnostic>,
}

impl Workspace {
    /// Resolve one directory into a workspace: read the global settings, apply
    /// the project's only when its trust decision allows it, overlay the
    /// caller's overrides, and report what was skipped instead of failing.
    pub fn discover(cwd: &Path, options: &WorkspaceOptions) -> Result<Self, Fault> {
        let cwd = canonical_dir(cwd)?;
        let global_dir = absolute(&options.global_dir)?;
        let global_path = global_dir.join("settings.json");
        let mut settings = read_settings(&global_path)?;
        resolve_resource_paths(&mut settings, &global_dir)?;
        let entries = read_trust(&global_dir)?;
        let saved = entries
            .iter()
            .filter(|entry| cwd.starts_with(&entry.root))
            .max_by_key(|entry| entry.root.components().count());
        let trusted = options
            .project_trust
            .unwrap_or_else(|| saved.is_some_and(|entry| entry.trusted));
        let mut diagnostics = vec![];
        let project_path = cwd.join(".eden/settings.json");
        if trusted {
            let mut project = read_settings(&project_path)?;
            resolve_resource_paths(&mut project, &cwd.join(".eden"))?;
            merge(&mut settings, project);
        } else if project_path.exists()
            || cwd.join(".eden/skills").exists()
            || cwd.join(".agents/skills").exists()
        {
            diagnostics.push(Diagnostic::warning(format!(
                "Untrusted project resources ignored: {}",
                cwd.display()
            )));
        }
        if !options.overrides.is_object() {
            return Err(invalid("CLI settings must be a JSON object"));
        }
        let mut overrides = options.overrides.clone();
        resolve_resource_paths(&mut overrides, &cwd)?;
        merge(&mut settings, overrides);
        Ok(Self {
            cwd,
            global_dir,
            trusted,
            settings,
            diagnostics,
        })
    }
}

/// Persist an explicit path grant or denial without loading project configuration.
pub fn save_trust(global_dir: &Path, root: &Path, trusted: bool) -> Result<(), Fault> {
    let root = canonical_dir(root)?;
    let global_dir = absolute(global_dir)?;
    std::fs::create_dir_all(&global_dir).map_err(io_error)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(global_dir.join("trust.lock"))
        .map_err(io_error)?;
    lock.lock().map_err(io_error)?;
    let mut entries = read_trust(&global_dir)?;
    entries.retain(|entry| entry.root != root);
    entries.push(TrustEntry { root, trusted });
    let temporary = global_dir.join("trust.json.next");
    let mut file = std::fs::File::create(&temporary).map_err(io_error)?;
    use std::io::Write;
    file.write_all(
        &serde_json::to_vec_pretty(&entries).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(&temporary, global_dir.join("trust.json")).map_err(io_error)
}

fn read_trust(global_dir: &Path) -> Result<Vec<TrustEntry>, Fault> {
    let bytes = match std::fs::read(global_dir.join("trust.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(io_error(error)),
    };
    let entries: Vec<TrustEntry> =
        serde_json::from_slice(&bytes).map_err(|error| invalid(format!("trust.json: {error}")))?;
    if entries.iter().any(|entry| {
        !entry.root.is_absolute()
            || entry
                .root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
    }) {
        return Err(invalid(
            "saved trust roots must be canonical absolute paths",
        ));
    }
    Ok(entries)
}

/// Resolve a path to a real directory, so every later comparison is made on
/// what the filesystem actually holds rather than on how it was spelled.
pub fn canonical_dir(path: &Path) -> Result<PathBuf, Fault> {
    let base = std::env::current_dir().map_err(io_error)?;
    let path = std::fs::canonicalize(paths::resolve_path(&base, path)?).map_err(io_error)?;
    if !path.is_dir() {
        return Err(invalid(format!("not a directory: {}", path.display())));
    }
    Ok(path)
}
fn absolute(path: &Path) -> Result<PathBuf, Fault> {
    paths::resolve_path(&std::env::current_dir().map_err(io_error)?, path)
}

fn read_settings(path: &Path) -> Result<Value, Fault> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({}));
        }
        Err(error) => return Err(io_error(error)),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
    if !value.is_object() {
        return Err(invalid(format!(
            "{}: settings must be an object",
            path.display()
        )));
    }
    Ok(value)
}
fn resolve_resource_paths(settings: &mut Value, root: &Path) -> Result<(), Fault> {
    for key in ["skills", "templates"] {
        if let Some(paths) = settings.get_mut(key) {
            for path in paths
                .as_array_mut()
                .ok_or_else(|| invalid(format!("{key} must be an array")))?
            {
                let text = path
                    .as_str()
                    .ok_or_else(|| invalid(format!("{key} entries must be paths")))?;
                *path = serde_json::json!(paths::resolve_path(root, Path::new(text))?);
            }
        }
    }
    Ok(())
}
/// Nested objects merge; arrays and scalars replace the previous value.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(old) => merge(old, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "workspace", message)
}
fn io_error(error: std::io::Error) -> Fault {
    Fault::new("FileFailure", "workspace", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eden-bootstrap-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("project/.eden")).unwrap();
            std::fs::create_dir_all(path.join("global")).unwrap();
            // Persisted trust roots use the canonical identity, including Windows
            // verbatim/long paths and macOS temporary-directory aliases.
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn options(&self) -> WorkspaceOptions {
            WorkspaceOptions {
                global_dir: self.0.join("global"),
                project_trust: None,
                overrides: json!({}),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn untrusted_project_settings_are_not_even_parsed() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("project/.eden/settings.json"),
            "not JSON; $(touch side-effect)",
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("global/settings.json"),
            r#"{"retry":{"max":3},"tools":["read"]}"#,
        )
        .unwrap();
        let workspace =
            Workspace::discover(&fixture.0.join("project"), &fixture.options()).unwrap();
        assert!(!workspace.trusted);
        assert_eq!(workspace.settings["tools"], json!(["read"]));
        assert!(!workspace.diagnostics.is_empty());
    }
    #[test]
    fn trusted_project_merges_objects_replaces_arrays_and_cli_wins() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("global/settings.json"),
            r#"{"retry":{"max":3,"delay":2},"tools":["read"]}"#,
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("project/.eden/settings.json"),
            r#"{"retry":{"max":4},"tools":["grep"]}"#,
        )
        .unwrap();
        let mut options = fixture.options();
        options.project_trust = Some(true);
        options.overrides = json!({ "retry": { "delay": 9 } });
        let workspace = Workspace::discover(&fixture.0.join("project"), &options).unwrap();
        assert_eq!(
            workspace.settings,
            json!({ "retry": { "max": 4, "delay": 9 }, "tools": ["grep"] })
        );
        assert!(workspace.trusted);
    }
    #[test]
    fn configured_paths_resolve_at_their_source_before_merging() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("global/settings.json"),
            r#"{"skills":["shared"]}"#,
        )
        .unwrap();
        let workspace =
            Workspace::discover(&fixture.0.join("project"), &fixture.options()).unwrap();
        assert_eq!(
            Path::new(workspace.settings["skills"][0].as_str().unwrap()),
            fixture.0.join("global").join("shared")
        );
    }
    #[test]
    fn nearest_denial_overrides_parent_trust_and_cli_can_override_once() {
        let fixture = Fixture::new();
        let entries = vec![
            TrustEntry {
                root: fixture.0.clone(),
                trusted: true,
            },
            TrustEntry {
                root: fixture.0.join("project"),
                trusted: false,
            },
        ];
        std::fs::write(
            fixture.0.join("global/trust.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let options = fixture.options();
        assert!(
            Workspace::discover(&fixture.0.join("global"), &options)
                .unwrap()
                .trusted,
            "the parent grant must match before the child denial can prove precedence"
        );
        assert!(
            !Workspace::discover(&fixture.0.join("project"), &options)
                .unwrap()
                .trusted
        );
        let mut once = options;
        once.project_trust = Some(true);
        assert!(
            Workspace::discover(&fixture.0.join("project"), &once)
                .unwrap()
                .trusted
        );
    }
}
