use super::manager::{error, io};
use eden_plugin_sdk::protocol::Fault;
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
};
pub(crate) fn relative(path: &Path) -> Result<&Path, Fault> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(error(
            "InvalidInput",
            format!("package path must stay inside its root: {}", path.display()),
        ));
    }
    Ok(path)
}
pub(crate) fn component(value: &str) -> Result<&str, Fault> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(error(
            "InvalidInput",
            "package name/version must be a single portable path component",
        ));
    }
    Ok(value)
}
pub(crate) fn copy(
    source: &Path,
    destination: &Path,
    cancel: &eden_plugin_sdk::Cancellation,
) -> Result<(), Fault> {
    std::fs::create_dir_all(destination).map_err(io)?;
    for entry in std::fs::read_dir(source).map_err(io)? {
        super::source::check_cancel(cancel)?;
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        if name == ".git" || name == "target" || name == "receipt.json" {
            continue;
        }
        let kind = entry.file_type().map_err(io)?;
        let target = destination.join(name);
        if kind.is_symlink() {
            return Err(error(
                "InvalidInput",
                "package symlinks are not supported; package regular files explicitly",
            ));
        }
        if kind.is_dir() {
            copy(&entry.path(), &target, cancel)?;
        } else if kind.is_file() {
            use std::io::Write;
            let mut input = std::fs::File::open(entry.path()).map_err(io)?;
            let mut output = std::fs::File::create(target).map_err(io)?;
            let mut buffer = [0; 65536];
            loop {
                super::source::check_cancel(cancel)?;
                let n = input.read(&mut buffer).map_err(io)?;
                if n == 0 {
                    break;
                }
                output.write_all(&buffer[..n]).map_err(io)?;
            }
        } else {
            return Err(error("InvalidInput", "package contains a special file"));
        }
    }
    Ok(())
}
pub(crate) fn unpack(
    source: &Path,
    destination: &Path,
    cancel: &eden_plugin_sdk::Cancellation,
) -> Result<(), Fault> {
    std::fs::create_dir_all(destination).map_err(io)?;
    let mut probe = std::fs::File::open(source).map_err(io)?;
    let mut magic = [0; 2];
    let _ = probe.read(&mut magic).map_err(io)?;
    let input = std::fs::File::open(source).map_err(io)?;
    let reader: Box<dyn Read> = if magic == [0x1f, 0x8b] {
        Box::new(flate2::read::GzDecoder::new(input))
    } else {
        Box::new(input)
    };
    for entry in tar::Archive::new(reader).entries().map_err(io)? {
        super::source::check_cancel(cancel)?;
        let mut entry = entry.map_err(io)?;
        let path = entry.path().map_err(io)?.into_owned();
        relative(&path)?;
        if !entry.header().entry_type().is_file() && !entry.header().entry_type().is_dir() {
            return Err(error(
                "InvalidInput",
                "archive links and special files are not supported",
            ));
        }
        if !entry.unpack_in(destination).map_err(io)? {
            return Err(error(
                "InvalidInput",
                "archive entry leaves the package directory",
            ));
        }
    }
    Ok(())
}
pub(crate) fn bundle_root(root: &Path) -> Result<PathBuf, Fault> {
    if root.join("package.json").is_file() {
        return Ok(root.into());
    }
    let entries: Vec<_> = std::fs::read_dir(root)
        .map_err(io)?
        .collect::<Result<_, _>>()
        .map_err(io)?;
    if entries.len() == 1
        && entries[0].file_type().map_err(io)?.is_dir()
        && entries[0].path().join("package.json").is_file()
    {
        return Ok(entries[0].path());
    }
    Err(error(
        "InvalidInput",
        "package.json must be at archive root or inside its sole top-level directory",
    ))
}
pub(crate) fn digest(root: &Path, cancel: &eden_plugin_sdk::Cancellation) -> Result<String, Fault> {
    eden_workspace::packages::digest_with(root, &mut || super::source::check_cancel(cancel))
}
pub(crate) fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), Fault> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    file.write_all(
        &serde_json::to_vec_pretty(value).map_err(|e| error("InvalidInput", e.to_string()))?,
    )
    .map_err(io)?;
    file.sync_all().map_err(io)
}
