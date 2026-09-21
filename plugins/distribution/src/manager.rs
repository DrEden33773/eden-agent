//! Installed-package records, receipts and the install, list, resolve and remove operations.
use eden_plugin_sdk::{
    Cancellation,
    protocol::{
        Fault, PackageManifest,
        resources::{LockedResourcePackage, PackageResources},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Source {
    Local { path: PathBuf },
    Git { url: String, revision: String },
    Https { url: String, sha256: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Bundle {
    pub manifest: Option<PackageManifest>,
    #[serde(default)]
    pub resources: Option<PackageResources>,
    #[serde(default)]
    pub dependencies: Vec<Source>,
    pub build: Option<Build>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Build {
    pub manifest_path: PathBuf,
    pub artifact: PathBuf,
}
pub(crate) struct Manager {
    pub root: PathBuf,
    pub client: reqwest::Client,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub path: PathBuf,
    pub manifest: Option<PackageManifest>,
    #[serde(default)]
    pub resources: Option<PackageResources>,
    pub source: Value,
    pub digest: String,
    pub dependencies: Vec<PathBuf>,
}
struct Prepared {
    directory: PathBuf,
    receipt: Receipt,
}
struct Staging(PathBuf);
struct OperationLock(std::fs::File);
impl Drop for OperationLock {
    fn drop(&mut self) {
        // Closing only this descriptor leaves a forked child's copy holding the
        // same lock until exec. Completion must release the shared lock now.
        let _ = self.0.unlock();
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub(crate) fn error(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "distribution", message)
}
pub(crate) fn io(error_value: std::io::Error) -> Fault {
    error("PackageFailure", error_value.to_string())
}
fn identity<'a>(
    native: Option<&'a PackageManifest>,
    resources: Option<&'a PackageResources>,
) -> Result<(&'a str, &'a str), Fault> {
    match (native, resources) {
        (Some(native), Some(resources))
            if native.descriptor.package != resources.name
                || native.descriptor.version != resources.version =>
        {
            Err(error(
                "InvalidInput",
                "native and resource package identities must match",
            ))
        }
        (Some(native), _) => Ok((&native.descriptor.package, &native.descriptor.version)),
        (None, Some(resources)) => Ok((&resources.name, &resources.version)),
        (None, None) => Err(error(
            "InvalidInput",
            "package needs a native manifest or resources",
        )),
    }
}
impl Manager {
    fn installed_path(&self, name: &str, version: &str) -> Result<PathBuf, Fault> {
        let base = self.root.join("packages").join(name).join(version);
        let native = base.join(eden_plugin_sdk::abi::TARGET);
        let resources = base.join("resources");
        match (native.exists(), resources.exists()) {
            (true, true) => Err(error(
                "VersionConflict",
                "version has both native and text-only identities",
            )),
            (true, false) => Ok(native),
            (false, true) => Ok(resources),
            (false, false) => Err(error(
                "MissingPackage",
                format!("{name}@{version} is not installed"),
            )),
        }
    }
    fn lock(&self) -> Result<OperationLock, Fault> {
        std::fs::create_dir_all(&self.root).map_err(io)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("operation.lock"))
            .map_err(io)?;
        file.try_lock()
            .map_err(|e| error("PackageBusy", e.to_string()))?;
        Ok(OperationLock(file))
    }
    pub(crate) async fn install(
        &self,
        source: Source,
        build: bool,
        cancel: Cancellation,
    ) -> Result<Value, Fault> {
        super::source::check_cancel(&cancel)?;
        let _lock = self.lock()?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = self.root.join(format!(
            ".stage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).map_err(io)?;
        let staging = Staging(root);
        let mut prepared = vec![];
        let receipt = self
            .stage(
                source,
                build,
                &cancel,
                &staging.0,
                &mut prepared,
                &mut std::collections::BTreeSet::new(),
            )
            .await?;
        // Validate the entire graph before publishing any new version.
        for package in &prepared {
            let path = package.receipt.path.join("receipt.json");
            if path.exists() {
                let installed: Receipt = serde_json::from_slice(&std::fs::read(path).map_err(io)?)
                    .map_err(|e| error("PackageFailure", e.to_string()))?;
                eden_workspace::packages::verify(&package.receipt.path)?;
                if installed.digest != package.receipt.digest {
                    return Err(error(
                        "VersionConflict",
                        "same package version and target already contain different bytes",
                    ));
                }
            } else if package.receipt.path.exists() {
                return Err(error(
                    "PackageFailure",
                    "existing package directory has no receipt",
                ));
            }
        }
        super::source::check_cancel(&cancel)?;
        let mut published = vec![];
        for package in prepared {
            if package.receipt.path.exists() {
                continue;
            }
            let result = (|| {
                let parent = package
                    .receipt
                    .path
                    .parent()
                    .ok_or_else(|| error("InvalidInput", "invalid package destination"))?;
                std::fs::create_dir_all(parent).map_err(io)?;
                super::files::write_json(
                    &package.directory.join("receipt.json"),
                    &package.receipt,
                )?;
                std::fs::rename(&package.directory, &package.receipt.path).map_err(io)
            })();
            if let Err(error) = result {
                for path in published {
                    let _ = std::fs::remove_dir_all(path);
                }
                return Err(error);
            }
            published.push(package.receipt.path);
        }
        serde_json::to_value(receipt).map_err(|e| error("InvalidInput", e.to_string()))
    }
    fn stage<'a>(
        &'a self,
        source: Source,
        build: bool,
        cancel: &'a Cancellation,
        root: &'a std::path::Path,
        prepared: &'a mut Vec<Prepared>,
        active: &'a mut std::collections::BTreeSet<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Receipt, Fault>> + Send + 'a>>
    {
        Box::pin(async move {
            use super::{files, source as sources};
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let slot = root.join(format!(
                "input-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let (source_root, locked_source) =
                sources::prepare(&source, &slot, cancel, &self.client).await?;
            let mut bundle: Bundle = serde_json::from_slice(
                &std::fs::read(source_root.join("package.json")).map_err(io)?,
            )
            .map_err(|e| error("InvalidInput", format!("package.json: {e}")))?;
            let (name, version) = identity(bundle.manifest.as_ref(), bundle.resources.as_ref())?;
            let name = files::component(name)?.to_owned();
            let version = files::component(version)?.to_owned();
            let identity = format!("{name}@{version}");
            if !active.insert(identity.clone()) {
                return Err(error("DependencyCycle", identity));
            }
            if let Some(manifest) = &bundle.manifest {
                if manifest.host != eden_plugin_sdk::protocol::CONTRACT
                    || manifest.sdk != eden_plugin_sdk::protocol::CONTRACT
                {
                    return Err(error(
                        "IncompatibleContract",
                        "package host or SDK mismatch",
                    ));
                }
                if !build && manifest.target != eden_plugin_sdk::abi::TARGET {
                    return Err(error(
                        "BuildRequired",
                        "no matching native target; explicitly build this plugin from source",
                    ));
                }
                files::relative(std::path::Path::new(&manifest.library))?;
            }
            let mut dependencies = vec![];
            for dependency in &bundle.dependencies {
                let mut dependency = dependency.clone();
                if let Source::Local { path } = &mut dependency
                    && !path.is_absolute()
                {
                    *path = source_root.join(&path);
                }
                dependencies.push(
                    self.stage(dependency, build, cancel, root, prepared, active)
                        .await?
                        .path,
                );
            }
            let output = root.join(format!(
                "output-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            files::copy(&source_root, &output, cancel)?;
            if build && bundle.manifest.is_some() {
                let spec = bundle.build.as_ref().ok_or_else(|| {
                    error(
                        "BuildRequired",
                        "package has no explicit plugin build definition",
                    )
                })?;
                let manifest = source_root.join(files::relative(&spec.manifest_path)?);
                let artifact = source_root.join(files::relative(&spec.artifact)?);
                let metadata = sources::command(
                    "cargo",
                    &[
                        "metadata",
                        "--no-deps",
                        "--format-version",
                        "1",
                        "--locked",
                        "--manifest-path",
                        &manifest.to_string_lossy(),
                    ],
                    &source_root,
                    cancel,
                )
                .await?;
                let metadata: Value = serde_json::from_str(&metadata)
                    .map_err(|e| error("InvalidInput", format!("plugin metadata: {e}")))?;
                let canonical = std::fs::canonicalize(&manifest).map_err(io)?;
                let package = metadata["packages"]
                    .as_array()
                    .and_then(|packages| {
                        packages.iter().find(|package| {
                            package["manifest_path"]
                                .as_str()
                                .and_then(|path| std::fs::canonicalize(path).ok())
                                .as_ref()
                                == Some(&canonical)
                        })
                    })
                    .ok_or_else(|| {
                        error(
                            "InvalidInput",
                            "build manifest must select one plugin package",
                        )
                    })?;
                let package_name = package["name"]
                    .as_str()
                    .ok_or_else(|| error("InvalidInput", "plugin package has no name"))?;
                if !package["targets"].as_array().is_some_and(|targets| {
                    targets.iter().any(|target| {
                        target["crate_types"]
                            .as_array()
                            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
                    })
                }) {
                    return Err(error(
                        "InvalidInput",
                        "plugin build must define a cdylib target",
                    ));
                }
                sources::command(
                    "cargo",
                    &[
                        "build",
                        "--release",
                        "--locked",
                        "--lib",
                        "--package",
                        package_name,
                        "--manifest-path",
                        &manifest.to_string_lossy(),
                    ],
                    &source_root,
                    cancel,
                )
                .await?;
                let library = std::path::Path::new("lib").join(
                    artifact
                        .file_name()
                        .ok_or_else(|| error("InvalidInput", "build artifact has no filename"))?,
                );
                std::fs::create_dir_all(output.join("lib")).map_err(io)?;
                std::fs::copy(&artifact, output.join(&library)).map_err(io)?;
                if let Some(manifest) = &mut bundle.manifest {
                    manifest.library = library.to_string_lossy().replace('\\', "/");
                    manifest.target = eden_plugin_sdk::abi::TARGET.into();
                }
                std::fs::remove_file(output.join("package.json")).map_err(io)?;
                files::write_json(&output.join("package.json"), &bundle)?;
            }
            if bundle
                .manifest
                .as_ref()
                .is_some_and(|manifest| !output.join(&manifest.library).is_file())
            {
                return Err(error(
                    "BuildRequired",
                    concat!(
                        "matching plugin artifact is missing; source build requires an explicit install ",
                        "--build",
                    ),
                ));
            }
            if let Some(resources) = &bundle.resources {
                eden_workspace::packages::validate_resource_paths(&output, resources)?;
            }
            let digest = files::digest(&output, cancel)?;
            let destination = self.root.join("packages").join(name).join(version).join(
                if bundle.manifest.is_some() {
                    eden_plugin_sdk::abi::TARGET
                } else {
                    "resources"
                },
            );
            let alternate = destination
                .parent()
                .ok_or_else(|| error("InvalidInput", "invalid destination"))?
                .join(if bundle.manifest.is_some() {
                    "resources"
                } else {
                    eden_plugin_sdk::abi::TARGET
                });
            if alternate.exists()
                || prepared
                    .iter()
                    .any(|package| package.receipt.path == alternate)
            {
                return Err(error(
                    "VersionConflict",
                    "same version cannot change between text-only and native bundle identity",
                ));
            }
            let receipt = Receipt {
                path: destination,
                manifest: bundle.manifest,
                resources: bundle.resources,
                source: locked_source,
                digest,
                dependencies,
            };
            if let Some(existing) = prepared.iter().find(|p| p.receipt.path == receipt.path) {
                if existing.receipt.digest != receipt.digest {
                    return Err(error(
                        "VersionConflict",
                        "dependency graph resolves one version to different bytes",
                    ));
                }
            } else {
                prepared.push(Prepared {
                    directory: output,
                    receipt: receipt.clone(),
                });
            }
            active.remove(&identity);
            Ok(receipt)
        })
    }
    pub(crate) fn list(&self) -> Result<Vec<Receipt>, Fault> {
        let mut output = vec![];
        let root = self.root.join("packages");
        if !root.exists() {
            return Ok(output);
        }
        fn visit(path: &std::path::Path, output: &mut Vec<Receipt>) -> Result<(), Fault> {
            if path.join("receipt.json").is_file() {
                output.push(
                    serde_json::from_slice(&std::fs::read(path.join("receipt.json")).map_err(io)?)
                        .map_err(|e| error("PackageFailure", e.to_string()))?,
                );
                return Ok(());
            }
            for entry in std::fs::read_dir(path).map_err(io)? {
                let entry = entry.map_err(io)?;
                if entry.file_type().map_err(io)?.is_dir() {
                    visit(&entry.path(), output)?;
                }
            }
            Ok(())
        }
        visit(&root, &mut output)?;
        output.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(output)
    }
    pub(crate) fn resolve(
        &self,
        base: eden_plugin_sdk::protocol::Composition,
        selections: &Value,
        roles: &Value,
    ) -> Result<Value, Fault> {
        use sha2::{Digest, Sha256};
        let _lock = self.lock()?;
        let mut composition = base;
        let mut pending: Vec<PathBuf> = vec![];
        for selection in selections
            .as_array()
            .ok_or_else(|| error("InvalidInput", "packages must be an array"))?
        {
            let name = selection["name"]
                .as_str()
                .ok_or_else(|| error("InvalidInput", "package name is required"))?;
            let version = selection["version"]
                .as_str()
                .ok_or_else(|| error("InvalidInput", "package version is required"))?;
            super::files::component(name)?;
            super::files::component(version)?;
            pending.push(self.installed_path(name, version)?);
        }
        let mut seen = std::collections::BTreeMap::new();
        while let Some(path) = pending.pop() {
            eden_workspace::packages::verify(&path)?;
            let receipt: Receipt =
                serde_json::from_slice(&std::fs::read(path.join("receipt.json")).map_err(io)?)
                    .map_err(|e| error("InvalidInput", e.to_string()))?;
            let name = identity(receipt.manifest.as_ref(), receipt.resources.as_ref())?
                .0
                .to_owned();
            if let Some(previous) = seen.insert(name.clone(), path.clone()) {
                if previous != path {
                    return Err(error(
                        "VersionConflict",
                        format!("composition requests multiple versions of {name}"),
                    ));
                }
                continue;
            }
            pending.extend(receipt.dependencies);
            composition
                .resource_packages
                .retain(|p| p.manifest.name != name);
            if let Some(resources) = receipt.resources {
                composition.resource_packages.push(LockedResourcePackage {
                    manifest: resources,
                    root: path.to_string_lossy().into_owned(),
                    digest: receipt.digest,
                });
            }
            if let Some(mut manifest) = receipt.manifest {
                manifest.library = path.join(&manifest.library).to_string_lossy().into_owned();
                if let Some(previous) = composition
                    .packages
                    .iter()
                    .find(|p| p.descriptor.package == name)
                {
                    manifest.config = previous.config.clone();
                }
                composition
                    .packages
                    .retain(|p| p.descriptor.package != name);
                composition.packages.push(manifest);
            } else {
                composition
                    .packages
                    .retain(|p| p.descriptor.package != name);
            }
        }
        for (role, name) in roles
            .as_object()
            .ok_or_else(|| error("InvalidInput", "roles must be an object"))?
        {
            composition.roles.insert(
                role.clone(),
                name.as_str()
                    .ok_or_else(|| error("InvalidInput", "role selection must be a package name"))?
                    .into(),
            );
        }
        for (role, name) in &composition.roles {
            if !composition
                .packages
                .iter()
                .any(|p| p.descriptor.package == *name && p.descriptor.provides.contains(role))
            {
                return Err(error(
                    "MissingDependency",
                    format!("{name} does not provide {role}"),
                ));
            }
        }
        for package in &composition.packages {
            for required in &package.requires {
                if !composition.roles.contains_key(required) {
                    return Err(error("MissingDependency", required));
                }
            }
        }
        eden_workspace::packages::validate_resource_packages(&composition.resource_packages)?;
        let bytes =
            serde_json::to_vec(&composition).map_err(|e| error("InvalidInput", e.to_string()))?;
        let id = format!("{:x}", Sha256::digest(&bytes));
        let directory = self.root.join("compositions");
        std::fs::create_dir_all(&directory).map_err(io)?;
        let path = directory.join(format!("{id}.json"));
        if path.exists() {
            let existing: eden_plugin_sdk::protocol::Composition =
                serde_json::from_slice(&std::fs::read(&path).map_err(io)?).map_err(|e| {
                    error(
                        "PackageIntegrity",
                        format!("saved composition is invalid: {e}"),
                    )
                })?;
            if serde_json::to_value(existing).map_err(|e| error("InvalidInput", e.to_string()))?
                != serde_json::to_value(&composition)
                    .map_err(|e| error("InvalidInput", e.to_string()))?
            {
                return Err(error(
                    "PackageIntegrity",
                    "saved composition differs from its identity",
                ));
            }
        } else {
            let temporary = directory.join(format!(".{id}-{}.tmp", std::process::id()));
            let result = (|| {
                super::files::write_json(&temporary, &composition)?;
                std::fs::rename(&temporary, &path).map_err(io)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result?;
        }
        Ok(json!({ "path": path, "id": id, "composition": composition }))
    }
    pub(crate) fn remove(&self, name: &str, version: &str, force: bool) -> Result<Value, Fault> {
        let _lock = self.lock()?;
        super::files::component(name)?;
        super::files::component(version)?;
        let target = self.installed_path(name, version)?;
        if !target.join("receipt.json").is_file() {
            return Err(error(
                "MissingPackage",
                "requested installed version was not found",
            ));
        }
        let mut references: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|r| r.dependencies.contains(&target))
            .map(|r| r.path.display().to_string())
            .collect();
        let compositions = self.root.join("compositions");
        if compositions.exists() {
            for entry in std::fs::read_dir(compositions).map_err(io)? {
                let path = entry.map_err(io)?.path();
                if !path.is_file() {
                    continue;
                }
                let composition: eden_plugin_sdk::protocol::Composition =
                    serde_json::from_slice(&std::fs::read(&path).map_err(io)?)
                        .map_err(|e| error("PackageFailure", e.to_string()))?;
                if composition
                    .packages
                    .iter()
                    .any(|p| std::path::Path::new(&p.library).starts_with(&target))
                    || composition
                        .resource_packages
                        .iter()
                        .any(|p| std::path::Path::new(&p.root).starts_with(&target))
                {
                    references.push(path.display().to_string());
                }
            }
        }
        references.extend(eden_workspace::packages::references(&self.root, &target)?);
        if !references.is_empty() && !force {
            return Err(error(
                "PackageInUse",
                format!(
                    concat!(
                        "version is referenced by {}; explicitly remove with force only after handling ",
                        "these bindings",
                    ),
                    references.join(", ")
                ),
            ));
        }
        std::fs::remove_dir_all(&target).map_err(io)?;
        Ok(json!({ "removed": target, "affected_bindings": references, "history_deleted": false }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eden-package-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn bundle(&self, version: &str, library: &[u8]) -> Source {
            let path = self.0.join(format!("source-{version}"));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("plugin.bin"), library).unwrap();
            let bundle = json!({
                "manifest": {
                    "descriptor": {
                        "package": "example",
                        "version": version,
                        "provides": ["example.service.v1"],
                    },
                    "host": eden_plugin_sdk::protocol::CONTRACT,
                    "sdk": eden_plugin_sdk::protocol::CONTRACT,
                    "target": eden_plugin_sdk::abi::TARGET,
                    "library": "plugin.bin",
                    "config": null,
                },
                "dependencies": [],
                "build": null,
            });
            std::fs::write(
                path.join("package.json"),
                serde_json::to_vec(&bundle).unwrap(),
            )
            .unwrap();
            Source::Local { path }
        }
        fn manager(&self) -> Manager {
            Manager {
                root: self.0.join("store"),
                client: super::super::source::client(None).unwrap(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn operation_completion_releases_lock_even_if_child_inherits_handle() {
        let f = Fixture::new();
        let manager = f.manager();
        let operation = manager.lock().unwrap();
        // A duplicated file description models the handle retained between fork and exec.
        let inherited = operation.0.try_clone().unwrap();
        drop(operation);
        let next = manager.lock();
        assert!(
            next.is_ok(),
            "completed operation retained lock: {:?}",
            next.err()
        );
        drop(inherited);
    }
    #[tokio::test]
    async fn text_package_installs_without_library_and_locks_versions_and_references() {
        let f = Fixture::new();
        let m = f.manager();
        let source = f.0.join("text-source");
        std::fs::create_dir_all(source.join("skills/check")).unwrap();
        std::fs::write(
            source.join("skills/check/SKILL.md"),
            "---\ndescription: check\n---\nVERSION ONE",
        )
        .unwrap();
        std::fs::write(
            source.join("package.json"),
            serde_json::to_vec(&json!({
                "resources": {
                    "name": "text-only",
                    "version": "1.0.0",
                    "skills": ["skills/check"],
                    "templates": [],
                },
            }))
            .unwrap(),
        )
        .unwrap();
        let installed = m
            .install(
                Source::Local {
                    path: source.clone(),
                },
                false,
                Cancellation::default(),
            )
            .await
            .unwrap();
        assert!(installed["manifest"].is_null());
        let empty = serde_json::from_value(json!({ "packages": [], "roles": {} })).unwrap();
        let resolved = m
            .resolve(
                empty,
                &json!([{ "name": "text-only", "version": "1.0.0" }]),
                &json!({}),
            )
            .unwrap();
        assert_eq!(resolved["composition"]["packages"], json!([]));
        assert_eq!(
            resolved["composition"]["resource_packages"][0]["digest"],
            installed["digest"]
        );
        let composition = serde_json::from_value(resolved["composition"].clone()).unwrap();
        eden_workspace::packages::validate(&composition, &m.root).unwrap();
        let history = f.0.join("text-history.jsonl");
        std::fs::write(&history, "saved history").unwrap();
        eden_workspace::packages::register(&m.root, &history, &composition).unwrap();
        std::fs::remove_dir_all(m.root.join("compositions")).unwrap();
        assert!(
            m.remove("text-only", "1.0.0", false)
                .unwrap_err()
                .message
                .contains("text-history.jsonl")
        );
        std::fs::write(
            source.join("package.json"),
            serde_json::to_vec(&json!({
                "resources": {
                    "name": "text-only",
                    "version": "2.0.0",
                    "skills": ["skills/check"],
                    "templates": [],
                },
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            source.join("skills/check/SKILL.md"),
            "---\ndescription: check\n---\nVERSION TWO",
        )
        .unwrap();
        m.install(
            Source::Local { path: source },
            true,
            Cancellation::default(),
        )
        .await
        .unwrap();
        eden_workspace::packages::validate(&composition, &m.root).unwrap();
        assert!(
            std::fs::read_to_string(
                PathBuf::from(installed["path"].as_str().unwrap()).join("skills/check/SKILL.md")
            )
            .unwrap()
            .contains("VERSION ONE")
        );
        m.remove("text-only", "1.0.0", true).unwrap();
        assert!(eden_workspace::packages::validate(&composition, &m.root).is_err());
        assert_eq!(std::fs::read_to_string(history).unwrap(), "saved history");
    }
    #[tokio::test]
    async fn versions_coexist_and_same_version_cannot_change_bytes() {
        let f = Fixture::new();
        let m = f.manager();
        let first = f.bundle("1.0.0", b"one");
        let installed = m
            .install(first.clone(), false, Cancellation::default())
            .await
            .unwrap();
        assert!(
            installed["path"]
                .as_str()
                .is_some_and(|p| std::path::Path::new(p).join("plugin.bin").is_file())
        );
        let second = m
            .install(f.bundle("2.0.0", b"two"), false, Cancellation::default())
            .await
            .unwrap();
        assert_ne!(installed["path"], second["path"]);
        assert!(
            m.install(
                f.bundle("1.0.0", b"changed"),
                false,
                Cancellation::default()
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read(
                std::path::Path::new(installed["path"].as_str().unwrap()).join("plugin.bin")
            )
            .unwrap(),
            b"one"
        );
    }
    #[tokio::test]
    async fn resolve_rejects_changed_installed_bytes_and_remove_reports_sessions() {
        let f = Fixture::new();
        let m = f.manager();
        let installed = m
            .install(f.bundle("1.0.0", b"one"), false, Cancellation::default())
            .await
            .unwrap();
        let root = PathBuf::from(installed["path"].as_str().unwrap());
        let empty = eden_plugin_sdk::protocol::Composition {
            resource_packages: vec![],
            packages: vec![],
            roles: Default::default(),
        };
        let resolved = m
            .resolve(
                empty.clone(),
                &json!([{ "name": "example", "version": "1.0.0" }]),
                &json!({ "example.service.v1": "example" }),
            )
            .unwrap();
        let composition = serde_json::from_value(resolved["composition"].clone()).unwrap();
        let history = f.0.join("saved.jsonl");
        std::fs::write(&history, "history").unwrap();
        eden_workspace::packages::register(&m.root, &history, &composition).unwrap();
        std::fs::remove_dir_all(m.root.join("compositions")).unwrap();
        assert!(
            m.remove("example", "1.0.0", false)
                .unwrap_err()
                .message
                .contains("saved.jsonl")
        );
        std::fs::write(root.join("plugin.bin"), "changed").unwrap();
        assert_eq!(
            m.resolve(
                empty,
                &json!([{ "name": "example", "version": "1.0.0" }]),
                &json!({})
            )
            .unwrap_err()
            .code,
            "PackageIntegrity"
        );
        assert!(eden_workspace::packages::validate(&composition, &m.root).is_err());
        m.remove("example", "1.0.0", true).unwrap();
        assert_eq!(std::fs::read_to_string(history).unwrap(), "history");
    }
    #[tokio::test]
    async fn mixed_package_resolves_native_and_resource_roles_and_rejects_escaped_roots() {
        let f = Fixture::new();
        let m = f.manager();
        let source = f.bundle("1.0.0", b"native bytes");
        let Source::Local { path } = &source else {
            unreachable!()
        };
        std::fs::create_dir(path.join("prompts")).unwrap();
        std::fs::write(path.join("prompts/check.md"), "Check $1").unwrap();
        let mut bundle: Value =
            serde_json::from_slice(&std::fs::read(path.join("package.json")).unwrap()).unwrap();
        bundle["resources"] = json!({
            "name": "example",
            "version": "1.0.0",
            "templates": ["prompts"],
        });
        std::fs::write(
            path.join("package.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        let installed = m
            .install(source.clone(), false, Cancellation::default())
            .await
            .unwrap();
        let empty = serde_json::from_value(json!({ "packages": [], "roles": {} })).unwrap();
        let resolved = m
            .resolve(
                empty,
                &json!([{ "name": "example", "version": "1.0.0" }]),
                &json!({ "example.service.v1": "example" }),
            )
            .unwrap();
        assert_eq!(
            resolved["composition"]["packages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            resolved["composition"]["resource_packages"][0]["manifest"]["templates"],
            json!(["prompts"])
        );
        let mut locked = serde_json::from_value::<eden_plugin_sdk::protocol::Composition>(
            resolved["composition"].clone(),
        )
        .unwrap();
        locked.resource_packages[0].digest = "changed".into();
        assert!(eden_workspace::packages::validate(&locked, &m.root).is_err());
        assert_eq!(installed["resources"]["name"], "example");
        bundle["resources"]["templates"] = json!(["../outside"]);
        std::fs::write(
            path.join("package.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        assert!(
            m.install(source, false, Cancellation::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn text_package_git_install_uses_the_selected_commit() {
        let f = Fixture::new();
        let source = f.0.join("git-source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join("package.json"),
            serde_json::to_vec(&json!({
                "resources": { "name": "git-text", "version": "1.0.0", "templates": ["check.md"] },
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(source.join("check.md"), "FROZEN $1").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&source)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        let revision = git(&["rev-parse", "HEAD"]);
        std::fs::write(source.join("check.md"), "UNCOMMITTED CHANGE").unwrap();
        let installed = f
            .manager()
            .install(
                Source::Git {
                    url: source.to_string_lossy().into_owned(),
                    revision: revision.clone(),
                },
                false,
                Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(installed["source"]["commit"], revision);
        let root = PathBuf::from(installed["path"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(root.join("check.md")).unwrap(),
            "FROZEN $1"
        );
    }
    #[tokio::test]
    async fn cancelled_local_install_never_publishes() {
        let f = Fixture::new();
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(
            f.manager()
                .install(f.bundle("1.0.0", b"one"), false, cancel)
                .await
                .unwrap_err()
                .code,
            "Cancelled"
        );
        assert!(!f.0.join("store/packages").exists());
    }
    #[tokio::test]
    async fn missing_artifact_requires_explicit_build_without_publishing() {
        let f = Fixture::new();
        let source = f.bundle("1.0.0", b"one");
        if let Source::Local { path } = &source {
            std::fs::remove_file(path.join("plugin.bin")).unwrap();
        }
        let error = f
            .manager()
            .install(source, false, Cancellation::default())
            .await
            .unwrap_err();
        assert_eq!(error.code, "BuildRequired");
        assert!(!f.0.join("store/packages/example").exists());
    }
}
