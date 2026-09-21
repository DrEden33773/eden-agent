//! Shared lexical path interpretation before each caller applies its trust or filesystem checks.
use eden_protocol::Fault;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The operating-system user home, independent of the application state directory.
/// Windows prefers USERPROFILE; other platforms prefer HOME.
pub fn user_home() -> Option<PathBuf> {
    home_with(|key| std::env::var_os(key), cfg!(windows))
}
fn home_with(env: impl Fn(&str) -> Option<OsString>, windows: bool) -> Option<PathBuf> {
    let keys = if windows {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    keys.into_iter()
        .filter_map(env)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Expand the current-user home and, on Windows, Bash drive paths, then resolve
/// relative paths against the caller's base. This does not canonicalize, guess
/// names, check existence or grant trust; those remain the caller's next step.
pub fn resolve_path(base: &Path, path: &Path) -> Result<PathBuf, Fault> {
    resolve_with(base, path, user_home().as_deref(), cfg!(windows))
}
fn resolve_with(
    base: &Path,
    path: &Path,
    home: Option<&Path>,
    windows: bool,
) -> Result<PathBuf, Fault> {
    let Some(text) = path.to_str() else {
        return Ok(base.join(path));
    };
    if text.is_empty() {
        return Err(Fault::new("InvalidInput", "path", "path must not be empty"));
    }
    if text == "~" || text.starts_with("~/") || (windows && text.starts_with("~\\")) {
        let home = home.ok_or_else(|| {
            Fault::new(
                "InvalidInput",
                "path",
                "user home is unavailable for ~ expansion",
            )
        })?;
        return Ok(if text == "~" {
            home.to_owned()
        } else {
            home.join(&text[2..])
        });
    }
    if windows {
        let normalized = text.replace('\\', "/");
        let drive_path = text
            .starts_with('/')
            .then(|| {
                normalized
                    .strip_prefix("/cygdrive/")
                    .or_else(|| normalized.strip_prefix("/mnt/"))
                    .or_else(|| normalized.strip_prefix('/'))
            })
            .flatten();
        if let Some(tail) = drive_path {
            let bytes = tail.as_bytes();
            if bytes.first().is_some_and(u8::is_ascii_alphabetic)
                && (bytes.len() == 1 || bytes.get(1) == Some(&b'/'))
            {
                return Ok(PathBuf::from(format!(
                    "{}:/{}",
                    (bytes[0] as char).to_ascii_uppercase(),
                    tail.get(2..).unwrap_or("")
                )));
            }
        }
        let bytes = normalized.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            if bytes.get(2) != Some(&b'/') {
                return Err(Fault::new(
                    "InvalidInput",
                    "path",
                    "drive-relative paths require an explicit drive root",
                ));
            }
            return Ok(PathBuf::from(normalized));
        }
    }
    Ok(base.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_rooted_backslash_path_is_not_a_bash_drive_path() {
        let base = Path::new("D:/project");
        let native = Path::new(r"\c\file");
        assert_eq!(
            resolve_with(base, native, None, true).unwrap(),
            base.join(native)
        );
    }
    #[test]
    fn home_expansion_does_not_follow_application_state_location() {
        let home = home_with(
            |key| match key {
                "HOME" => Some("/user-home".into()),
                "EDEN_AGENT_DIR" => Some("/custom/state".into()),
                _ => None,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            resolve_with(
                Path::new("/project"),
                Path::new("~/folder/new.txt"),
                Some(&home),
                false
            )
            .unwrap(),
            PathBuf::from("/user-home/folder/new.txt")
        );
        assert_eq!(
            resolve_with(
                Path::new("/project"),
                Path::new("~other/file"),
                Some(&home),
                false
            )
            .unwrap(),
            PathBuf::from("/project/~other/file")
        );
    }
    #[test]
    fn windows_drive_spellings_preserve_spaces_unicode_and_exact_names() {
        for input in [
            "/c/目录 with spaces/file.txt",
            "/cygdrive/c/目录 with spaces/file.txt",
            "/mnt/c/目录 with spaces/file.txt",
            "C:\\目录 with spaces\\file.txt",
        ] {
            assert_eq!(
                resolve_with(Path::new("D:/project"), Path::new(input), None, true).unwrap(),
                PathBuf::from("C:/目录 with spaces/file.txt")
            );
        }
        assert!(
            resolve_with(
                Path::new("D:/project"),
                Path::new("C:ambiguous"),
                None,
                true
            )
            .is_err()
        );
        assert_eq!(
            resolve_with(Path::new("/project"), Path::new("/c/file"), None, false).unwrap(),
            PathBuf::from("/c/file")
        );
    }
    #[test]
    fn missing_home_is_an_error_only_for_current_user_expansion() {
        assert!(resolve_with(Path::new("/project"), Path::new("~/file"), None, false).is_err());
        assert!(resolve_with(Path::new("/project"), Path::new("relative"), None, false).is_ok());
        assert_eq!(
            home_with(
                |key| Some(
                    if key == "USERPROFILE" {
                        "C:/Users/native"
                    } else {
                        "/git/home"
                    }
                    .into()
                ),
                true
            ),
            Some(PathBuf::from("C:/Users/native"))
        );
    }
}
