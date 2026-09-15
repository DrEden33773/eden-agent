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
        let root = receipt_root(library);
        let Some(root) = root else {
            if packages.as_ref().is_some_and(|p| library.starts_with(p)) {
                return Err(fail("installed package has no receipt"));
            }
            continue;
        };
        let receipt = verify(root)?;
        let mut expected: PackageManifest =
            serde_json::from_value(receipt["manifest"].clone()).map_err(fail)?;
        expected.library = std::fs::canonicalize(root.join(&expected.library))
            .map_err(fail)?
            .to_string_lossy()
            .into_owned();
        expected.config = package.config.clone();
        if serde_json::to_value(expected).map_err(fail)?
            != serde_json::to_value(package).map_err(fail)?
        {
            return Err(fail("composition differs from installed manifest"));
        }
    }
    Ok(())
}
pub fn register(store: &Path, history: &Path, composition: &Composition) -> Result<(), Fault> {
    let mut stores = std::collections::BTreeSet::new();
    let packages = std::fs::canonicalize(store.join("packages")).ok();
    if packages.as_ref().is_some_and(|root| {
        composition
            .packages
            .iter()
            .any(|p| Path::new(&p.library).starts_with(root))
    }) {
        stores.insert(std::fs::canonicalize(store).map_err(fail)?);
    }
    for package in &composition.packages {
        if let Some(root) = receipt_root(Path::new(&package.library)) {
            let prefix = root.ancestors().nth(3);
            if let Some(prefix) = prefix.filter(|p| p.file_name().is_some_and(|n| n == "packages"))
            {
                stores.insert(
                    prefix
                        .parent()
                        .ok_or_else(|| fail("invalid store root"))?
                        .to_owned(),
                );
            }
        }
    }
    for store in stores {
        register_at(&store, history, composition)?;
    }
    Ok(())
}
fn receipt_root(library: &Path) -> Option<&Path> {
    library
        .parent()?
        .ancestors()
        .find(|p| p.join("receipt.json").is_file())
}
fn register_at(store: &Path, history: &Path, composition: &Composition) -> Result<(), Fault> {
    let path = std::fs::canonicalize(history).map_err(fail)?;
    let libraries: Vec<_> = composition.packages.iter().map(|p| &p.library).collect();
    let directory = store.join("sessions");
    std::fs::create_dir_all(&directory).map_err(fail)?;
    let id = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    let target = directory.join(format!("{id}.json"));
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
