use eden_plugin_sdk::{
    Cancellation,
    protocol::{Fault, PackageManifest},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Local { path: PathBuf },
    Git { url: String, revision: String },
    Https { url: String, sha256: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub manifest: PackageManifest,
    #[serde(default)]
    pub dependencies: Vec<Source>,
    pub build: Option<Build>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Build {
    pub manifest_path: PathBuf,
    pub artifact: PathBuf,
}
pub struct Manager {
    pub root: PathBuf,
    pub client: reqwest::Client,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub path: PathBuf,
    pub manifest: PackageManifest,
    pub source: Value,
    pub digest: String,
    pub dependencies: Vec<PathBuf>,
}
struct Prepared {
    directory: PathBuf,
    receipt: Receipt,
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub fn error(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "distribution", message)
}
pub fn io(error_value: std::io::Error) -> Fault {
    error("PackageFailure", error_value.to_string())
}
impl Manager {
    fn lock(&self) -> Result<std::fs::File, Fault> {
        std::fs::create_dir_all(&self.root).map_err(io)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("operation.lock"))
            .map_err(io)?;
        file.try_lock()
            .map_err(|e| error("PackageBusy", e.to_string()))?;
        Ok(file)
    }
    pub async fn install(
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
            let name = files::component(&bundle.manifest.descriptor.package)?.to_owned();
            let version = files::component(&bundle.manifest.descriptor.version)?.to_owned();
            let identity = format!("{name}@{version}");
            if !active.insert(identity.clone()) {
                return Err(error("DependencyCycle", identity));
            }
            if bundle.manifest.host != eden_plugin_sdk::protocol::CONTRACT
                || bundle.manifest.sdk != eden_plugin_sdk::protocol::CONTRACT
            {
                return Err(error(
                    "IncompatibleContract",
                    "package host or SDK mismatch",
                ));
            }
            if !build && bundle.manifest.target != eden_plugin_sdk::abi::TARGET {
                return Err(error(
                    "BuildRequired",
                    "no matching native target; explicitly build this plugin from source",
                ));
            }
            files::relative(std::path::Path::new(&bundle.manifest.library))?;
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
            if build {
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
                bundle.manifest.library = library.to_string_lossy().replace('\\', "/");
                bundle.manifest.target = eden_plugin_sdk::abi::TARGET.into();
                std::fs::remove_file(output.join("package.json")).map_err(io)?;
                files::write_json(&output.join("package.json"), &bundle)?;
            }
            if !output.join(&bundle.manifest.library).is_file() {
                return Err(error(
                    "BuildRequired",
                    "matching plugin artifact is missing; source build requires an explicit \
                install --build",
                ));
            }
            let digest = files::digest(&output, cancel)?;
            let destination = self
                .root
                .join("packages")
                .join(name)
                .join(version)
                .join(eden_plugin_sdk::abi::TARGET);
            let receipt = Receipt {
                path: destination,
                manifest: bundle.manifest,
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
    pub fn list(&self) -> Result<Vec<Receipt>, Fault> {
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
    pub fn resolve(
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
            pending.push(
                self.root
                    .join("packages")
                    .join(name)
                    .join(version)
                    .join(eden_plugin_sdk::abi::TARGET),
            );
        }
        let mut seen = std::collections::BTreeMap::new();
        while let Some(path) = pending.pop() {
            eden_workspace::packages::verify(&path)?;
            let receipt: Receipt =
                serde_json::from_slice(&std::fs::read(path.join("receipt.json")).map_err(io)?)
                    .map_err(|e| error("InvalidInput", e.to_string()))?;
            let name = receipt.manifest.descriptor.package.clone();
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
            let mut manifest = receipt.manifest;
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
        Ok(json!({
        "path":path,
        "id":id,
        "composition":composition
        }))
    }
    pub fn remove(&self, name: &str, version: &str, force: bool) -> Result<Value, Fault> {
        let _lock = self.lock()?;
        super::files::component(name)?;
        super::files::component(version)?;
        let target = self
            .root
            .join("packages")
            .join(name)
            .join(version)
            .join(eden_plugin_sdk::abi::TARGET);
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
                    "version is referenced by {}; explicitly remove with force only after \
                handling these bindings",
                    references.join(", ")
                ),
            ));
        }
        std::fs::remove_dir_all(&target).map_err(io)?;
        Ok(json!({
        "removed":target,
        "affected_bindings":references,
        "history_deleted":false
        }))
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
                        "manifest":{
                        "descriptor":{
                        "package":"example",
                        "version":version,
                        "provides":["example.service.v1"]
            },
                        "host":eden_plugin_sdk::protocol::CONTRACT,
                        "sdk":eden_plugin_sdk::protocol::CONTRACT,
                        "target":eden_plugin_sdk::abi::TARGET,
                        "library":"plugin.bin",
                        "config":null
            },
                        "dependencies":[],
                        "build":null
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
            packages: vec![],
            roles: Default::default(),
        };
        let resolved = m
            .resolve(
                empty.clone(),
                &json!([{
                "name":"example",
                "version":"1.0.0"
                }]),
                &json!({
                "example.service.v1":"example"
                }),
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
                &json!([{
                "name":"example",
                "version":"1.0.0"
                }]),
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
