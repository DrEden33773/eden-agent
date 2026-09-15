//! Data-only validation and reference records for immutable installed packages.
use eden_protocol::{Composition, Fault, PackageManifest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
fn fail(e: impl std::fmt::Display) -> Fault {
    Fault::new("PackageIntegrity", "packages", e.to_string())
}

pub fn digest(root: &Path) -> Result<String, Fault> {
    digest_with(root, &mut || Ok(()))
}
pub fn digest_with(
    root: &Path,
    check: &mut dyn FnMut() -> Result<(), Fault>,
) -> Result<String, Fault> {
    fn visit(
        root: &Path,
        path: &Path,
        hash: &mut Sha256,
        check: &mut dyn FnMut() -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        let mut entries = std::fs::read_dir(path)
            .map_err(fail)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail)?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            check()?;
            let path = entry.path();
            if path == root.join("receipt.json") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(fail)?
                .to_str()
                .ok_or_else(|| fail("package paths must be UTF-8"))?
                .replace('\\', "/");
            let kind = entry.file_type().map_err(fail)?;
            hash.update(if kind.is_dir() { b"d" } else { b"f" });
            hash.update((rel.len() as u64).to_le_bytes());
            hash.update(rel.as_bytes());
            if kind.is_dir() {
                visit(root, &path, hash, check)?;
            } else if kind.is_file() {
                let mut file = std::fs::File::open(path).map_err(fail)?;
                let mut content = Sha256::new();
                let mut buffer = [0; 65536];
                loop {
                    check()?;
                    let n = file.read(&mut buffer).map_err(fail)?;
                    if n == 0 {
                        break;
                    }
                    content.update(&buffer[..n]);
                }
                hash.update(content.finalize());
            } else {
                return Err(fail("package contains a link or special file"));
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    hash.update(b"eden-package-tree-v1");
    visit(root, root, &mut hash, check)?;
    Ok(format!("{:x}", hash.finalize()))
}
pub fn verify(root: &Path) -> Result<Value, Fault> {
    if std::fs::symlink_metadata(root)
        .map_err(fail)?
        .file_type()
        .is_symlink()
    {
        return Err(fail("installed package root is a link"));
    }
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(root.join("receipt.json")).map_err(fail)?)
            .map_err(fail)?;
    if receipt["digest"].as_str() != Some(&digest(root)?) {
        return Err(fail(format!(
            "installed package changed: {}",
            root.display()
        )));
    }
    let manifest: PackageManifest =
        serde_json::from_value(receipt["manifest"].clone()).map_err(fail)?;
    let library = std::fs::canonicalize(root.join(&manifest.library)).map_err(fail)?;
    if !library.starts_with(std::fs::canonicalize(root).map_err(fail)?) {
        return Err(fail("installed library escapes its package"));
    }
    Ok(receipt)
}
pub fn validate(composition: &Composition, store: &Path) -> Result<(), Fault> {
    let packages = store.join("packages");
    let packages = std::fs::canonicalize(packages).ok();
    for package in &composition.packages {
        let library = Path::new(&package.library);
        let resolved = std::fs::canonicalize(library).ok();
        let Some(managed) = managed_owner(library)? else {
            if packages.as_ref().is_some_and(|p| {
                library.starts_with(p) || resolved.as_ref().is_some_and(|l| l.starts_with(p))
            }) {
                return Err(fail("installed package has no receipt"));
            }
            continue;
        };
        let root = &managed.root;
        let receipt = verify(root)?;
        let mut expected: PackageManifest =
            serde_json::from_value(receipt["manifest"].clone()).map_err(fail)?;
        expected.library = std::fs::canonicalize(root.join(&expected.library))
            .map_err(fail)?
            .to_string_lossy()
            .into_owned();
        expected.config = package.config.clone();
        let mut actual = package.clone();
        actual.library = resolved
            .ok_or_else(|| fail("installed library is missing"))?
            .to_string_lossy()
            .into_owned();
        if serde_json::to_value(expected).map_err(fail)?
            != serde_json::to_value(actual).map_err(fail)?
        {
            return Err(fail("composition differs from installed manifest"));
        }
    }
    Ok(())
}
/// Preserve the declared location until managed integrity has been checked.
/// Canonicalizing first would turn a tampered managed symlink into a loose library.
pub fn resolve_paths(
    composition: &mut Composition,
    base: &Path,
    store: &Path,
) -> Result<(), Fault> {
    for package in &mut composition.packages {
        package.library = base.join(&package.library).to_string_lossy().into_owned();
    }
    validate(composition, store)?;
    for package in &mut composition.packages {
        package.library = std::fs::canonicalize(&package.library)
            .map_err(|e| {
                Fault::new(
                    "MissingDependency",
                    &package.descriptor.package,
                    e.to_string(),
                )
            })?
            .to_string_lossy()
            .into_owned();
    }
    Ok(())
}
pub fn register(store: &Path, history: &Path, composition: &Composition) -> Result<(), Fault> {
    register_pending(store, history, composition, false)
}
/// Before a binding commit, retain both the previous and proposed libraries.
/// After it commits, replace the reference with the committed composition.
pub fn register_pending(
    store: &Path,
    history: &Path,
    composition: &Composition,
    pending: bool,
) -> Result<(), Fault> {
    register_libraries(
        store,
        history,
        composition
            .packages
            .iter()
            .map(|p| p.library.clone())
            .collect(),
        pending,
    )
}
/// Track a copied history's saved locations without loading its native libraries.
pub fn register_libraries(
    store: &Path,
    history: &Path,
    libraries: Vec<String>,
    pending: bool,
) -> Result<(), Fault> {
    // Saved compositions can use aliases such as macOS /var or Windows paths
    // without a verbatim prefix. Reference membership uses physical identities.
    let mut stores = std::collections::BTreeSet::new();
    let mut retained = std::collections::BTreeSet::new();
    for library in libraries {
        let declared = PathBuf::from(library);
        let resolved = std::fs::canonicalize(&declared).unwrap_or_else(|_| declared.clone());
        retained.insert(resolved.to_string_lossy().into_owned());
        for path in [&declared, &resolved] {
            if let Some(managed) = managed_owner(path)? {
                let root = &managed.root;
                let owner = &managed.store;
                stores.insert(std::fs::canonicalize(owner).unwrap_or_else(|_| owner.to_owned()));
                // Keep the managed location even when a later symlink redirects
                // the saved library outside its original package.
                let root_identity = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
                retained.insert(
                    root_identity
                        .join(managed.library.strip_prefix(root).map_err(fail)?)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let libraries: Vec<String> = retained.into_iter().collect();
    let packages = std::fs::canonicalize(store.join("packages")).ok();
    if packages
        .as_ref()
        .is_some_and(|root| libraries.iter().any(|p| Path::new(p).starts_with(root)))
    {
        stores.insert(std::fs::canonicalize(store).map_err(fail)?);
    }
    for store in stores {
        register_at(&store, history, &libraries, pending)?;
    }
    Ok(())
}
struct ManagedOwner {
    root: PathBuf,
    store: PathBuf,
    library: PathBuf,
}
fn directory_owner(directory: &Path) -> Option<(&Path, &Path)> {
    // The manager publishes <store>/packages/<name>/<version>/<target>/... .
    // Receipt existence or contents cannot define ownership: missing/corrupt
    // receipts must stay managed failures, and unrelated ancestor files are data.
    let ancestors: Vec<_> = directory.ancestors().collect();
    ancestors.into_iter().rev().find_map(|root| {
        let packages = root.ancestors().nth(3)?;
        if packages.file_name().is_some_and(|name| name == "packages") {
            Some((root, packages.parent()?))
        } else {
            None
        }
    })
}
fn managed_owner(library: &Path) -> Result<Option<ManagedOwner>, Fault> {
    // Follow each redirect separately, from outside in, including the final
    // file. canonicalize() could jump over managed A on a chain leading to B.
    let mut location = std::path::absolute(library).map_err(fail)?;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..256 {
        if !seen.insert(location.clone()) {
            return Err(fail("cyclic native library path"));
        }
        let prefixes: Vec<_> = location.ancestors().collect();
        let mut redirected = None;
        for prefix in prefixes.into_iter().rev() {
            if let Some((root, store)) = directory_owner(prefix) {
                return Ok(Some(ManagedOwner {
                    root: root.to_owned(),
                    store: store.to_owned(),
                    library: location.clone(),
                }));
            }
            if std::fs::symlink_metadata(prefix).is_ok_and(|m| m.file_type().is_symlink()) {
                let target = std::fs::read_link(prefix).map_err(fail)?;
                let target = prefix.parent().unwrap_or(Path::new("")).join(target);
                let suffix = location.strip_prefix(prefix).map_err(fail)?;
                redirected = Some(if suffix.as_os_str().is_empty() {
                    target
                } else {
                    target.join(suffix)
                });
                break;
            }
        }
        let Some(next) = redirected else {
            return Ok(None);
        };
        location = next;
    }
    // Never downgrade unresolved ownership to a loose library on a redirect loop.
    Err(fail("too many native library redirects"))
}
fn register_at(
    store: &Path,
    history: &Path,
    libraries: &[String],
    pending: bool,
) -> Result<(), Fault> {
    let path = std::fs::canonicalize(history).map_err(fail)?;
    let mut libraries: std::collections::BTreeSet<String> = libraries.iter().cloned().collect();
    let directory = store.join("sessions");
    std::fs::create_dir_all(&directory).map_err(fail)?;
    let id = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    let target = directory.join(format!("{id}.json"));
    if pending && target.exists() {
        let previous: Value =
            serde_json::from_slice(&std::fs::read(&target).map_err(fail)?).map_err(fail)?;
        for library in previous["libraries"]
            .as_array()
            .ok_or_else(|| fail("invalid session reference"))?
        {
            libraries.insert(
                library
                    .as_str()
                    .ok_or_else(|| fail("invalid library reference"))?
                    .to_owned(),
            );
        }
    }
    let temporary = directory.join(format!("{id}-{}.tmp", std::process::id()));
    std::fs::write(
        &temporary,
        serde_json::to_vec(&json!({"history":path,"libraries":libraries})).map_err(fail)?,
    )
    .map_err(fail)?;
    std::fs::rename(temporary, target).map_err(fail)
}
pub fn references(store: &Path, target: &Path) -> Result<Vec<String>, Fault> {
    let mut output = vec![];
    let target = std::fs::canonicalize(target).map_err(fail)?;
    let root = store.join("sessions");
    if !root.exists() {
        return Ok(output);
    }
    for entry in std::fs::read_dir(root).map_err(fail)? {
        let path = entry.map_err(fail)?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let value: Value =
            serde_json::from_slice(&std::fs::read(path).map_err(fail)?).map_err(fail)?;
        let history = PathBuf::from(
            value["history"]
                .as_str()
                .ok_or_else(|| fail("invalid session reference"))?,
        );
        if history.exists()
            && value["libraries"]
                .as_array()
                .ok_or_else(|| fail("invalid libraries reference"))?
                .iter()
                .any(|p| {
                    p.as_str()
                        .is_some_and(|p| Path::new(p).starts_with(&target))
                })
        {
            output.push(history.display().to_string());
        }
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn directory_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fn file_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
    #[test]
    fn managed_redirect_keeps_integrity_and_copied_reference_through_directory_alias() {
        let root =
            std::env::temp_dir().join(format!("eden-package-redirect-{}", std::process::id()));
        let store = root.join("original-store");
        let package = store.join("packages/example/1.0.0/test");
        let outside = root.join("second-store/packages/example/1.0.0/test");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("library"), b"external library").unwrap();
        let target_manifest: PackageManifest = serde_json::from_value(json!({
            "descriptor":{"package":"example","version":"1.0.0","provides":[]},
            "library":"library","sdk":"test","host":"test","target":"test","config":null
        }))
        .unwrap();
        std::fs::write(
            outside.join("receipt.json"),
            serde_json::to_vec(&json!({
                "manifest":target_manifest,"digest":digest(&outside).unwrap()
            }))
            .unwrap(),
        )
        .unwrap();
        let mut direct = Composition {
            packages: vec![target_manifest],
            roles: Default::default(),
        };
        resolve_paths(&mut direct, &outside, &root.join("other-store")).unwrap();
        std::fs::write(package.join("receipt.json"), b"invalid receipt").unwrap();
        directory_link(&outside, &package.join("lib"));
        let alias = root.join("package-alias");
        directory_link(&package, &alias);
        let compound_alias = root.join("compound-alias");
        directory_link(&package.join("lib"), &compound_alias);
        file_link(&outside.join("library"), &package.join("file-link"));
        let loose = root.join("loose-library");
        file_link(&package.join("file-link"), &loose);
        let history = root.join("copied.jsonl");
        std::fs::write(&history, b"copied saved composition").unwrap();
        for library in [
            package.join("lib/library"),
            alias.join("lib/library"),
            compound_alias.join("library"),
            loose,
        ] {
            let mut composition: Composition = serde_json::from_value(json!({"packages":[{
                "descriptor":{"package":"example","version":"1.0.0","provides":[]},
                "library":library,"sdk":"test","host":"test","target":"test","config":null
            }],"roles":{}}))
            .unwrap();
            assert_eq!(
                resolve_paths(&mut composition, &root, &root.join("other-store"))
                    .unwrap_err()
                    .code,
                "PackageIntegrity"
            );
            register_libraries(
                &root.join("other-store"),
                &history,
                vec![library.to_string_lossy().into_owned()],
                false,
            )
            .unwrap();
            assert_eq!(references(&store, &package).unwrap().len(), 1);
        }
        let redirected_root = root.join("root-store/packages/example/1.0.0/test");
        std::fs::create_dir_all(redirected_root.parent().unwrap()).unwrap();
        directory_link(&outside, &redirected_root);
        assert_eq!(
            verify(&redirected_root).unwrap_err().code,
            "PackageIntegrity"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn package_ownership_ignores_unrelated_receipts_and_keeps_managed_failures_closed() {
        let root = std::env::temp_dir().join(format!("eden-package-owner-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let loose = root.join("installation/plugins/example/1.0.0/library");
        std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
        std::fs::write(&loose, b"loose library").unwrap();
        std::fs::write(
            root.join("receipt.json"),
            br#"{"download_digest":"unrelated"}"#,
        )
        .unwrap();
        let composition = |library: &Path| -> Composition {
            serde_json::from_value(json!({"packages":[{
                "descriptor":{"package":"example","version":"1.0.0","provides":[]},
                "library":library,"sdk":"test","host":"test","target":"test","config":null
            }],"roles":{}}))
            .unwrap()
        };
        let active_store = root.join("active-store");
        validate(&composition(&loose), &active_store).unwrap();
        for store in [&active_store, &root.join("other-store")] {
            let package = store.join("packages/example/1.0.0/test");
            let library = package.join("lib/library");
            std::fs::create_dir_all(library.parent().unwrap()).unwrap();
            std::fs::write(&library, b"managed library").unwrap();
            let mut manifest = composition(Path::new("lib/library")).packages.remove(0);
            manifest.library = "lib/library".into();
            let receipt = json!({"manifest":manifest,"digest":digest(&package).unwrap(),"path":"old-location-before-relocation"});
            let receipt_path = package.join("receipt.json");
            std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            validate(&composition(&library), &active_store).unwrap();
            let history = root.join("copied-session.jsonl");
            std::fs::write(&history, b"saved history").unwrap();
            register_libraries(
                &active_store,
                &history,
                vec![library.to_string_lossy().into_owned()],
                false,
            )
            .unwrap();
            assert_eq!(references(store, &package).unwrap().len(), 1);
            for bad in [
                Some(b"invalid".as_slice()),
                Some(br#"{"digest":"wrong"}"#.as_slice()),
                None,
            ] {
                if let Some(bytes) = bad {
                    std::fs::write(&receipt_path, bytes).unwrap();
                } else {
                    std::fs::remove_file(&receipt_path).unwrap();
                }
                assert_eq!(
                    validate(&composition(&library), &active_store)
                        .unwrap_err()
                        .code,
                    "PackageIntegrity"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pending_reference_preserves_both_bindings_until_commit() {
        let root = std::env::temp_dir().join(format!("eden-reference-{}", std::process::id()));
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::create_dir_all(root.join("new")).unwrap();
        std::fs::write(root.join("old/plugin"), b"old library").unwrap();
        std::fs::write(root.join("new/plugin"), b"new library").unwrap();
        let history = root.join("history.jsonl");
        std::fs::write(&history, b"original binding").unwrap();
        let composition = |version: &str| -> Composition {
            serde_json::from_value(json!({"packages":[{
                "descriptor":{"package":"test","version":"1","provides":[]},
                "library":std::fs::canonicalize(root.join(version).join("plugin")).unwrap(),"sdk":"test","host":"test","target":"test","config":null
            }],"roles":{}})).unwrap()
        };
        register_at(
            &root,
            &history,
            &[composition("old").packages[0].library.clone()],
            false,
        )
        .unwrap();
        register_at(
            &root,
            &history,
            &[composition("new").packages[0].library.clone()],
            true,
        )
        .unwrap();
        // A failed binding append leaves the pending union protecting the old history.
        assert_eq!(references(&root, &root.join("old")).unwrap().len(), 1);
        assert_eq!(references(&root, &root.join("new")).unwrap().len(), 1);
        std::fs::write(&history, b"new binding committed").unwrap();
        register_at(
            &root,
            &history,
            &[composition("new").packages[0].library.clone()],
            false,
        )
        .unwrap();
        assert!(references(&root, &root.join("old")).unwrap().is_empty());
        assert_eq!(references(&root, &root.join("new")).unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tree_encoding_is_unambiguous() {
        let root = std::env::temp_dir().join(format!("eden-digest-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), b"x\0b\0y").unwrap();
        let first = digest(&root).unwrap();
        std::fs::write(root.join("a"), b"x").unwrap();
        std::fs::write(root.join("b"), b"y").unwrap();
        assert_ne!(first, digest(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
