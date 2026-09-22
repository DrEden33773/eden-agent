//! Whole-installation transactions retain old directories and publish only validated starts.
use super::{
    files,
    manager::{Manager, Source, error, io},
    source,
};
use eden_plugin_sdk::{
    Cancellation, Package,
    protocol::{Fault, updates::*},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Deserialize, Default)]
struct Config {
    managed_root: Option<PathBuf>,
    #[serde(default)]
    sources: Vec<Tracking>,
}
#[derive(Clone, Deserialize)]
struct Tracking {
    target: UpdateTarget,
    source: Discovery,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Discovery {
    Github { repository: String, asset: String },
    Local { path: PathBuf },
}
#[derive(Serialize, Deserialize)]
struct ReleaseManifest {
    version: String,
    target: String,
    executable: String,
    files: BTreeMap<String, String>,
}
struct Updater {
    manager: Arc<Manager>,
    config: Config,
}
struct OperationLock(std::fs::File);
impl OperationLock {
    fn acquire(root: &Path) -> Result<Self, Fault> {
        std::fs::create_dir_all(root).map_err(io)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("update.lock"))
            .map_err(io)?;
        file.try_lock()
            .map_err(|e| error("UpdateBusy", e.to_string()))?;
        Ok(Self(file))
    }
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn attach(
    package: Package,
    manager: Arc<Manager>,
    config: &Value,
) -> Result<Package, Fault> {
    let config: Config =
        serde_json::from_value(config.get("updates").cloned().unwrap_or(json!({})))
            .map_err(|e| error("InvalidInput", e.to_string()))?;
    if config
        .managed_root
        .as_ref()
        .is_some_and(|p| !p.is_absolute())
    {
        return Err(error(
            "InvalidInput",
            "updates.managed_root must be absolute",
        ));
    }
    let updater = Arc::new(Updater { manager, config });
    Ok(
        package.service(UPDATE_SOURCE, move |request: UpdateRequest, cx| {
            let updater = updater.clone();
            async move {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let cancel = cx.scope.cancellation();
                cx.scope.spawn(async move {
                    let result = updater.dispatch(request, cancel).await;
                    let _ = sender.send(result);
                    Ok(())
                })?;
                receiver
                    .await
                    .map_err(|_| error("Unavailable", "update completion lost"))?
            }
        }),
    )
}

impl Updater {
    fn status(&self, target: UpdateTarget) -> Result<UpdateStatus, Fault> {
        let managed = target == UpdateTarget::Host && self.config.managed_root.is_some();
        let current_version = if target == UpdateTarget::Host {
            Some(env!("CARGO_PKG_VERSION").into())
        } else {
            None
        };
        Ok(UpdateStatus {
            configured: target == UpdateTarget::Host
                || self.config.sources.iter().any(|s| s.target == target),
            target,
            current_version,
            managed,
            instructions: if managed {
                "Prepare the complete release, then explicitly activate it. Existing sessions keep \
                 their installation."
            } else {
                "Use the original installation channel: rebuild and install a source checkout, or \
                 replace a manually installed complete directory. Plugin preparation does not \
                 enable it; use package.resolve for a new composition."
            }
            .into(),
            candidate: None,
        })
    }
    async fn dispatch(
        &self,
        request: UpdateRequest,
        cancel: Cancellation,
    ) -> Result<UpdateReply, Fault> {
        source::check_cancel(&cancel)?;
        match request {
            UpdateRequest::Discover => {
                let mut targets = vec![self.status(UpdateTarget::Host)?];
                for receipt in self.manager.list()? {
                    let name = receipt
                        .manifest
                        .as_ref()
                        .map(|m| &m.descriptor.package)
                        .or_else(|| receipt.resources.as_ref().map(|r| &r.name));
                    if let Some(name) = name {
                        let target = UpdateTarget::Plugin { name: name.clone() };
                        if !targets.iter().any(|s| s.target == target) {
                            let mut status = self.status(target)?;
                            status.current_version = receipt
                                .manifest
                                .as_ref()
                                .map(|m| m.descriptor.version.clone())
                                .or_else(|| receipt.resources.as_ref().map(|r| r.version.clone()));
                            targets.push(status);
                        }
                    }
                }
                Ok(UpdateReply::Discovered { targets })
            }
            UpdateRequest::Check { target, channel } => {
                let mut status = self.status(target.clone())?;
                let default = Discovery::Github {
                    repository: "DrEden33773/eden-agent".into(),
                    asset: format!("eden-agent-{}.tar.gz", eden_plugin_sdk::abi::TARGET),
                };
                let discovery = self
                    .config
                    .sources
                    .iter()
                    .find(|s| s.target == target)
                    .map(|s| &s.source)
                    .or_else(|| (target == UpdateTarget::Host).then_some(&default));
                if let Some(discovery) = discovery {
                    let releases = self.releases(discovery, &channel, &cancel).await?;
                    if let Some(release) = select_release(&releases, &channel) {
                        let version = release["tag_name"]
                            .as_str()
                            .ok_or_else(|| error("InvalidRelease", "missing release tag"))?
                            .to_owned();
                        let source = match discovery {
                            Discovery::Local { .. } => release["source"].clone(),
                            Discovery::Github { asset, .. } => {
                                let asset = release["assets"]
                                    .as_array()
                                    .and_then(|a| a.iter().find(|a| a["name"] == *asset))
                                    .ok_or_else(|| {
                                        error(
                                            "MissingAsset",
                                            "release has no complete archive for this target",
                                        )
                                    })?;
                                let digest = asset["digest"]
                                    .as_str()
                                    .and_then(|s| s.strip_prefix("sha256:"))
                                    .ok_or_else(|| {
                                        error(
                                            "InvalidRelease",
                                            "release asset needs GitHub SHA-256 digest",
                                        )
                                    })?;
                                json!({
                                    "kind": "https",
                                    "url": asset["browser_download_url"],
                                    "sha256": digest,
                                })
                            }
                        };
                        if status
                            .current_version
                            .as_deref()
                            .map(|s| s.trim_start_matches('v'))
                            != Some(version.trim_start_matches('v'))
                        {
                            status.candidate = Some(Candidate {
                                target,
                                version,
                                source,
                                channel,
                            });
                        }
                    }
                }
                Ok(UpdateReply::Checked { status })
            }
            UpdateRequest::Prepare { candidate } => self.prepare(candidate, cancel).await,
            UpdateRequest::Activate { prepared } => self.activate(prepared, &cancel).await,
        }
    }
    async fn releases(
        &self,
        discovery: &Discovery,
        channel: &Channel,
        cancel: &Cancellation,
    ) -> Result<Value, Fault> {
        match discovery {
            Discovery::Local { path } => serde_json::from_slice(&std::fs::read(path).map_err(io)?)
                .map_err(|e| error("InvalidRelease", e.to_string())),
            Discovery::Github { repository, .. } => {
                let parts: Vec<_> = repository.split('/').collect();
                if parts.len() != 2 {
                    return Err(error("InvalidInput", "repository must be owner/name"));
                }
                for part in parts {
                    files::component(part)?;
                }
                let mut url = reqwest::Url::parse(&format!(
                    "https://api.github.com/repos/{repository}/releases"
                ))
                .map_err(|e| error("InvalidInput", e.to_string()))?;
                match channel {
                    Channel::Stable => {
                        url.path_segments_mut()
                            .map_err(|_| error("InvalidInput", "invalid API URL"))?
                            .push("latest");
                    }
                    Channel::Tag { tag } => {
                        url.path_segments_mut()
                            .map_err(|_| error("InvalidInput", "invalid API URL"))?
                            .push("tags")
                            .push(tag);
                    }
                    Channel::Prerelease => {
                        url.set_query(Some("per_page=100"));
                    }
                }
                let request = async {
                    let response = self
                        .manager
                        .client
                        .get(url)
                        .header("User-Agent", "eden-agent")
                        .header("Accept", "application/vnd.github+json")
                        .send()
                        .await
                        .map_err(|e| error("UpdateCheckFailed", e.to_string()))?;
                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Ok(json!([]));
                    }
                    let bytes = response
                        .error_for_status()
                        .map_err(|e| error("UpdateCheckFailed", e.to_string()))?
                        .bytes()
                        .await
                        .map_err(|e| error("UpdateCheckFailed", e.to_string()))?;
                    serde_json::from_slice(&bytes)
                        .map_err(|e| error("InvalidRelease", e.to_string()))
                };
                tokio::select! {
                    _ = cancel.cancelled() => Err(error("Cancelled", "update check cancelled")),
                    result = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            request
                        ) =>
                        result.map_err(|_| error("UpdateCheckFailed", "update check timed out"))?,
                }
            }
        }
    }
    async fn prepare(
        &self,
        candidate: Candidate,
        cancel: Cancellation,
    ) -> Result<UpdateReply, Fault> {
        let input: Source = serde_json::from_value(candidate.source.clone())
            .map_err(|e| error("InvalidInput", e.to_string()))?;
        if let UpdateTarget::Plugin { ref name } = candidate.target {
            // Manager verifies package identities, checksums and dependency graph without activation.
            let receipt = self.manager.install(input, false, cancel).await?;
            let installed_name = receipt["manifest"]["descriptor"]["package"]
                .as_str()
                .or_else(|| receipt["resources"]["name"].as_str());
            let installed_version = receipt["manifest"]["descriptor"]["version"]
                .as_str()
                .or_else(|| receipt["resources"]["version"].as_str());
            if installed_name != Some(name) || installed_version != Some(&candidate.version) {
                return Err(error(
                    "InvalidRelease",
                    "prepared package does not match candidate identity; it remains disabled",
                ));
            }
            return Ok(UpdateReply::Prepared {
                prepared: PreparedUpdate {
                    target: candidate.target,
                    version: candidate.version,
                    path: receipt["path"].as_str().unwrap_or_default().into(),
                    digest: receipt["digest"].as_str().unwrap_or_default().into(),
                },
            });
        }
        let root = self.managed_root()?;
        let _lock = OperationLock::acquire(root)?;
        let staging = Staging(root.join("staging"));
        if staging.0.exists() {
            std::fs::remove_dir_all(&staging.0).map_err(io)?;
        }
        std::fs::create_dir(&staging.0).map_err(io)?;
        let (bundle, _) = source::prepare(
            &input,
            &staging.0.join("input"),
            &cancel,
            &self.manager.client,
        )
        .await?;
        if let Source::Local { path } = &input
            && path.is_dir()
        {
            copy_permissions(path, &bundle)?;
        }
        let manifest = read_manifest(&bundle)?;
        if manifest.version != candidate.version || manifest.target != eden_plugin_sdk::abi::TARGET
        {
            return Err(error(
                "InvalidRelease",
                "release identity or target does not match selection",
            ));
        }
        verify_files(&bundle, &manifest, &cancel)?;
        probe(&bundle, &manifest, &cancel).await?;
        let digest = files::digest(&bundle, &cancel)?;
        let destination = root.join("releases").join(format!(
            "{}-{}",
            files::component(&manifest.version)?,
            manifest.target
        ));
        std::fs::create_dir_all(root.join("releases")).map_err(io)?;
        source::check_cancel(&cancel)?;
        if destination.exists() {
            if files::digest(&destination, &cancel)? != digest {
                return Err(error(
                    "VersionConflict",
                    "release identity already has different bytes",
                ));
            }
        } else {
            std::fs::rename(&bundle, &destination).map_err(io)?;
        }
        Ok(UpdateReply::Prepared {
            prepared: PreparedUpdate {
                target: UpdateTarget::Host,
                version: manifest.version,
                path: destination.to_string_lossy().into_owned(),
                digest,
            },
        })
    }
    fn managed_root(&self) -> Result<&Path, Fault> {
        self.config.managed_root.as_deref().ok_or_else(|| {
            error(
                "ManualUpdateRequired",
                "use the original source or manual installation channel; automatic replacement is \
                 unavailable",
            )
        })
    }
    async fn activate(
        &self,
        prepared: PreparedUpdate,
        cancel: &Cancellation,
    ) -> Result<UpdateReply, Fault> {
        if prepared.target != UpdateTarget::Host {
            return Err(error(
                "ExplicitCompositionRequired",
                "use package.resolve to enable a prepared plugin in a new composition",
            ));
        }
        let root = self.managed_root()?;
        let _lock = OperationLock::acquire(root)?;
        let expected = root.join("releases").join(format!(
            "{}-{}",
            files::component(&prepared.version)?,
            eden_plugin_sdk::abi::TARGET
        ));
        if Path::new(&prepared.path) != expected
            || files::digest(&expected, cancel)? != prepared.digest
        {
            return Err(error(
                "UpdateIntegrity",
                "prepared installation changed or is outside this managed root",
            ));
        }
        let manifest = read_manifest(&expected)?;
        verify_files(&expected, &manifest, cancel)?;
        probe(&expected, &manifest, cancel).await?;
        let previous = active_installation(root)
            .map_err(io)?
            .map(|p| p.to_string_lossy().into_owned());
        let directory = root.join("activations");
        std::fs::create_dir_all(&directory).map_err(io)?;
        let next = std::fs::read_dir(&directory)
            .map_err(io)?
            .filter_map(Result::ok)
            .filter_map(|e| e.path().file_stem()?.to_str()?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| error("UpdateFailure", "activation sequence exhausted"))?;
        let pending = directory.join("pending.json");
        if pending.exists() {
            std::fs::remove_file(&pending).map_err(io)?;
        }
        files::write_json(
            &pending,
            &json!({
                "path": expected
                    .strip_prefix(root)
                    .map_err(|e| error("UpdateFailure", e.to_string()))?,
            }),
        )?;
        source::check_cancel(cancel)?;
        std::fs::rename(&pending, directory.join(format!("{next:020}.json"))).map_err(io)?;
        Ok(UpdateReply::Activated {
            path: prepared.path,
            previous,
        })
    }
}
fn select_release<'a>(releases: &'a Value, channel: &Channel) -> Option<&'a Value> {
    let matches = |release: &&Value| {
        release["draft"] != true
            && match channel {
                Channel::Stable => release["prerelease"] != true,
                Channel::Prerelease => true,
                Channel::Tag { tag } => release["tag_name"] == *tag,
            }
    };
    if let Some(releases) = releases.as_array() {
        releases.iter().find(matches)
    } else {
        std::iter::once(releases).find(matches)
    }
}
fn read_manifest(root: &Path) -> Result<ReleaseManifest, Fault> {
    serde_json::from_slice(&std::fs::read(root.join("release.json")).map_err(io)?)
        .map_err(|e| error("InvalidRelease", e.to_string()))
}
fn verify_files(
    root: &Path,
    manifest: &ReleaseManifest,
    cancel: &Cancellation,
) -> Result<(), Fault> {
    let mut actual = BTreeMap::new();
    fn walk(
        root: &Path,
        directory: &Path,
        actual: &mut BTreeMap<String, String>,
        cancel: &Cancellation,
    ) -> Result<(), Fault> {
        for entry in std::fs::read_dir(directory).map_err(io)? {
            source::check_cancel(cancel)?;
            let entry = entry.map_err(io)?;
            let kind = entry.file_type().map_err(io)?;
            if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
                return Err(error(
                    "InvalidRelease",
                    "release contains links or special files",
                ));
            }
            if kind.is_dir() {
                walk(root, &entry.path(), actual, cancel)?;
            } else {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .map_err(|e| error("InvalidRelease", e.to_string()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative != "release.json" {
                    actual.insert(
                        relative,
                        format!("{:x}", Sha256::digest(std::fs::read(path).map_err(io)?)),
                    );
                }
            }
        }
        Ok(())
    }
    walk(root, root, &mut actual, cancel)?;
    if actual != manifest.files || !manifest.files.contains_key(&manifest.executable) {
        return Err(error(
            "UpdateIntegrity",
            "complete release file inventory or digest mismatch",
        ));
    }
    files::relative(Path::new(&manifest.executable))?;
    Ok(())
}
async fn probe(
    root: &Path,
    manifest: &ReleaseManifest,
    cancel: &Cancellation,
) -> Result<(), Fault> {
    let executable = root.join(files::relative(Path::new(&manifest.executable))?);
    source::check_cancel(cancel)?;
    let probe_cancel = Cancellation::default();
    let executable = executable.to_string_lossy();
    let command = source::command(&executable, &["installation-check"], root, &probe_cancel);
    tokio::pin!(command);
    tokio::select! {
        result = &mut command => {
            result?;
            Ok(())
        },
        _ = cancel.cancelled() => {
            probe_cancel.cancel();
            let _ = command.await;
            Err(error("Cancelled", "installation check cancelled"))
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            probe_cancel.cancel();
            let _ = command.await;
            Err(error("UpdateCheckFailed", "installation check timed out"))
        }
    }
}
fn copy_permissions(source: &Path, destination: &Path) -> Result<(), Fault> {
    for entry in std::fs::read_dir(source).map_err(io)? {
        let entry = entry.map_err(io)?;
        let target = destination.join(entry.file_name());
        if !target.exists() {
            continue;
        }
        if entry.file_type().map_err(io)?.is_dir() {
            copy_permissions(&entry.path(), &target)?;
        } else {
            std::fs::set_permissions(target, entry.metadata().map_err(io)?.permissions())
                .map_err(io)?;
        }
    }
    Ok(())
}
#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
