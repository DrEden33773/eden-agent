//! Standalone services and immutable previews for clients that cannot open the original session.
use super::*;
use eden_protocol::{
    Composition,
    delivery::*,
    updates::{UPDATE_SOURCE, UpdateRequest},
};
use serde_json::json;

/// Only explicitly selected delivery packages are loaded, never the saved conversation binding.
/// Call `shutdown` even after a failed operation to complete native cleanup.
pub struct Delivery {
    kernel: Kernel,
    next: AtomicU64,
}
impl Delivery {
    /// Load selected delivery roles from an installation composition. Project configuration is
    /// deliberately not discovered; callers must explicitly supply a trusted composition.
    pub async fn open(composition: impl AsRef<Path>, roles: &[&str]) -> Result<Self, Fault> {
        Self::open_with_global(composition, roles, &WorkspaceOptions::default().global_dir).await
    }
    /// Use an explicit private global directory for independent distribution operations.
    pub async fn open_with_global(
        composition: impl AsRef<Path>,
        roles: &[&str],
        global: &Path,
    ) -> Result<Self, Fault> {
        let path = composition.as_ref();
        let mut selected: Composition =
            serde_json::from_slice(&std::fs::read(path).map_err(file_fault)?)
                .map_err(data_fault)?;
        if roles.is_empty()
            || roles
                .iter()
                .any(|r| !matches!(*r, EXPORTER | SHARE_TARGET | UPDATE_SOURCE))
        {
            return Err(Fault::new(
                "InvalidInput",
                "delivery",
                "select at least one delivery role",
            ));
        }
        selected
            .roles
            .retain(|role, _| roles.contains(&role.as_str()));
        for role in roles {
            if !selected.roles.contains_key(*role) {
                return Err(Fault::new("MissingDependency", "delivery", *role));
            }
        }
        selected.packages.retain(|p| {
            selected
                .roles
                .values()
                .any(|name| name == &p.descriptor.package)
        });
        selected.resource_packages.clear();
        for package in &mut selected.packages {
            if package.descriptor.package == "distribution" {
                if !package.config.is_object() {
                    package.config = json!({});
                }
                package.config["root"] = json!(global.join("distribution"));
                if package.config["updates"]["managed_root"].is_null()
                    && let Some(root) = std::env::var_os("EDEN_MANAGED_ROOT")
                {
                    package.config["updates"]["managed_root"] =
                        json!(std::path::PathBuf::from(root));
                }
            }
        }
        let kernel = Kernel::load_resolved(
            selected,
            path.parent().unwrap_or(Path::new(".")),
            0,
            Events::new(0),
        )
        .await?;
        Ok(Self {
            kernel,
            next: AtomicU64::new(1),
        })
    }
    /// Call an independent role with an explicit cancellation token. Publication is never retried.
    pub async fn invoke(
        &self,
        role: &str,
        payload: serde_json::Value,
        cancel: Cancellation,
    ) -> Result<serde_json::Value, Fault> {
        let terminal = self
            .kernel
            .invoke(
                Request {
                    session_id: 0,
                    run_id: self.next.fetch_add(1, Ordering::Relaxed),
                    contract: role.into(),
                    payload,
                },
                cancel,
            )
            .await;
        if !terminal.cleanup_errors.is_empty() {
            return Err(Fault::new(
                "CleanupFailure",
                "delivery",
                format!("{:?}", terminal.cleanup_errors),
            ));
        }
        match terminal.outcome {
            Outcome::Completed(value) => Ok(value),
            Outcome::Failed(error) => Err(error),
            Outcome::Cancelled if role == SHARE_TARGET => Err(publication_unknown()),
            Outcome::Cancelled => Err(Fault::new("Cancelled", "delivery", "operation cancelled")),
        }
    }
    /// Export a committed file independently of the business packages it names.
    pub async fn export_file(
        &self,
        path: impl AsRef<Path>,
        selection: Selection,
        format: Format,
    ) -> Result<Artifact, Fault> {
        self.export_file_with_cancel(path, selection, format, Cancellation::default())
            .await
    }
    /// Export with caller-owned cancellation; cleanup settles before the result is returned.
    pub async fn export_file_with_cancel(
        &self,
        path: impl AsRef<Path>,
        selection: Selection,
        format: Format,
        cancel: Cancellation,
    ) -> Result<Artifact, Fault> {
        let records = history::read(path.as_ref())?;
        serde_json::from_value(
            self.invoke(
                EXPORTER,
                json!(ExportRequest {
                    records,
                    selection,
                    format
                }),
                cancel,
            )
            .await?,
        )
        .map_err(data_fault)
    }
    /// Finish all native service scopes before the caller drops its runtime.
    pub async fn shutdown(&self) -> Result<(), Fault> {
        self.kernel.shutdown().await
    }
}
fn publication_unknown() -> Fault {
    Fault::new(
        "PublicationUnknown",
        "delivery",
        "publication cancelled; remote creation may have occurred; check your gists before \
         retrying",
    )
}
fn file_fault(e: std::io::Error) -> Fault {
    Fault::new("FileFailure", "delivery", e.to_string())
}
fn data_fault(e: serde_json::Error) -> Fault {
    Fault::new("InvalidInput", "delivery", e.to_string())
}
/// Save the exact preview plus a SHA-256 identity for an explicit later publication.
/// The destination must be a new file; the returned identity is supplied to `read_preview`.
pub fn save_preview(artifact: &Artifact, path: &Path) -> Result<String, Fault> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(file_fault)?;
    file.write_all(artifact.content.as_bytes())
        .map_err(file_fault)?;
    file.sync_all().map_err(file_fault)?;
    Ok(preview_digest(artifact.content.as_bytes()))
}
fn preview_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
/// Read only the explicitly reviewed artifact and reject edits since preview; never reread history.
pub fn read_preview(path: &Path, digest: &str) -> Result<Artifact, Fault> {
    let bytes = std::fs::read(path).map_err(file_fault)?;
    if preview_digest(&bytes) != digest {
        return Err(Fault::new(
            "StalePreview",
            "delivery",
            "artifact changed; prepare and preview again",
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|e| Fault::new("InvalidInput", "delivery", e.to_string()))?;
    if path.extension().is_some_and(|ext| ext == "html") {
        return Err(Fault::new(
            "InvalidInput",
            "delivery",
            "HTML export has been removed; use a .jsonl preview",
        ));
    }
    let header = content
        .lines()
        .next()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    if header
        .as_ref()
        .is_none_or(|value| value["format"] != "eden-reading-v1" || value["restorable"] != false)
    {
        return Err(Fault::new(
            "InvalidInput",
            "delivery",
            "share requires an eden-reading-v1 preview, not a history backup",
        ));
    }
    Ok(Artifact {
        filename: "conversation.jsonl".into(),
        media_type: "application/x-ndjson".into(),
        content,
        warnings: vec![],
    })
}
impl Session {
    /// Render one committed snapshot without writing a file or including streaming deltas.
    pub async fn export(&self, selection: Selection, format: Format) -> Result<Artifact, Fault> {
        self.service(
            0,
            EXPORTER,
            &ExportRequest {
                records: self.history().await?,
                selection,
                format,
            },
        )
        .await
    }
    /// Retain up to eight fixed previews so transports can publish large artifacts by identity.
    /// A client discards an unused preview explicitly; identical bytes reuse the same identity.
    pub async fn prepare_export(
        &self,
        selection: Selection,
        format: Format,
    ) -> Result<(String, Artifact), Fault> {
        let artifact = self.export(selection, format).await?;
        let id = preview_digest(&serde_json::to_vec(&artifact).map_err(data_fault)?);
        let mut previews = self.0.previews.lock().unwrap_or_else(|e| e.into_inner());
        if previews.len() >= 8 && !previews.contains_key(&id) {
            return Err(Fault::new(
                "PreviewLimit",
                "delivery",
                "discard an unused preview before preparing another",
            ));
        }
        previews.insert(id.clone(), artifact.clone());
        Ok((id, artifact))
    }
    /// Release an unused preview; publication never rereads conversation history.
    pub fn forget_preview(&self, id: &str) -> bool {
        self.0
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .is_some()
    }
    /// Publish the exact retained preview with explicit confirmation, without a transport-sized upload.
    pub fn publish_preview(&self, id: &str, confirmed: bool) -> Result<u64, Fault> {
        let artifact = self
            .0
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                Fault::new(
                    "UnknownPreview",
                    "delivery",
                    "preview missing; prepare and review again",
                )
            })?;
        self.publish(PublishRequest {
            artifact,
            confirmed,
        })
    }
    /// Publish previously previewed bytes as a cancellable owned operation, never fresh history.
    pub fn publish(&self, request: PublishRequest) -> Result<u64, Fault> {
        self.delivery_run(SHARE_TARGET, json!(request))
    }
    /// Explicit check, preparation or activation. Preparing does not change session bindings.
    pub fn update(&self, request: UpdateRequest) -> Result<u64, Fault> {
        self.delivery_run(UPDATE_SOURCE, json!(request))
    }
    fn delivery_run(&self, role: &'static str, payload: serde_json::Value) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            let mut terminal = session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: role.into(),
                        payload,
                    },
                    cancel,
                )
                .await;
            if role == SHARE_TARGET && matches!(terminal.outcome, Outcome::Cancelled) {
                terminal.outcome = Outcome::Failed(publication_unknown());
            }
            terminal
        })
    }
}
