//! Shell discovery resolves an executable before CreateProcess can choose System32.
use crate::{Fault, fault};
#[cfg(any(windows, test))]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The command language chosen by the caller; discovery never changes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    /// Native Bash, including Git for Windows.
    Bash,
    /// PowerShell Core or, for default discovery, Windows PowerShell.
    PowerShell,
}
/// The ownership domain the launcher can actually settle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellTransport {
    /// A native process group or Windows Job Object; no WSL forwarding.
    Native,
}
/// A discovered shell with an explicit language and cleanup transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedShell {
    /// The selected executable, resolved before launch so PATH cannot be shadowed.
    pub executable: PathBuf,
    /// The language requested by the caller, retained through executable fallback.
    pub kind: ShellKind,
    /// The process domain whose descendants the launcher owns.
    pub transport: ShellTransport,
}

/// Resolve a shell without spawning it. Explicit paths must exist and never fall
/// back. Default Windows Bash checks Git installation roots after PATH; default
/// PowerShell can fall back from pwsh to Windows PowerShell. Legacy WSL launchers
/// are refused because their Linux children are outside a Windows Job Object.
pub fn resolve_shell(
    shell: &str,
    kind: ShellKind,
    cwd: &Path,
    default: bool,
) -> Result<ResolvedShell, Fault> {
    #[cfg(windows)]
    let executable = windows_shell(
        shell,
        kind,
        cwd,
        default,
        |key| std::env::var_os(key),
        |path| path.is_file(),
    )?;
    #[cfg(not(windows))]
    let executable = {
        let _ = default;
        unix_shell(shell, cwd, &std::env::var_os("PATH").unwrap_or_default())?
    };
    Ok(ResolvedShell {
        executable,
        kind,
        transport: ShellTransport::Native,
    })
}
#[cfg(not(windows))]
fn unix_shell(shell: &str, cwd: &Path, search: &std::ffi::OsStr) -> Result<PathBuf, Fault> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    let path = Path::new(shell);
    if path.components().count() > 1 || path.is_absolute() {
        let resolved = cwd.join(path);
        return if executable(&resolved) {
            Ok(resolved)
        } else {
            Err(unavailable(shell))
        };
    }
    std::env::split_paths(search)
        .map(|directory| cwd.join(directory).join(path))
        .find(|candidate| executable(candidate))
        .ok_or_else(|| unavailable(shell))
}
fn unavailable(shell: &str) -> Fault {
    fault(
        "ShellUnavailable",
        format!("native shell executable {shell:?} was not found; configure its executable path"),
    )
}
#[cfg(any(windows, test))]
fn windows_shell(
    shell: &str,
    kind: ShellKind,
    cwd: &Path,
    default: bool,
    env: impl Fn(&str) -> Option<OsString>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, Fault> {
    let requested = Path::new(shell);
    let system = env("SystemRoot").map(PathBuf::from);
    let legacy = |path: &Path| {
        let normalize = |path: &Path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches("//?/")
                .to_ascii_lowercase()
        };
        let path = normalize(path);
        system.as_ref().is_some_and(|root| {
            ["System32", "Sysnative"]
                .iter()
                .any(|directory| path == normalize(&root.join(directory).join("bash.exe")))
        })
    };
    let checked = |path: PathBuf| -> Result<PathBuf, Fault> {
        // Canonicalization identifies explicit junction/symlink aliases of the WSL launcher.
        let actual = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if legacy(&actual) || legacy(&path) {
            Err(fault(
                "UnsupportedShellTransport",
                "legacy Windows WSL bash transport is unsupported: Linux descendants cannot be owned by the native Windows job; configure Git Bash",
            ))
        } else if exists(&path) {
            Ok(path)
        } else {
            Err(unavailable(shell))
        }
    };
    if requested.components().count() > 1 || requested.is_absolute() || shell.contains('\\') {
        return checked(cwd.join(requested));
    }
    let executable = if requested.extension().is_some() {
        requested.to_owned()
    } else {
        requested.with_extension("exe")
    };
    let search = std::env::split_paths(&env("PATH").unwrap_or_default()).collect::<Vec<_>>();
    let mut candidates = search
        .iter()
        .map(|directory| cwd.join(directory).join(&executable))
        .collect::<Vec<_>>();
    if default {
        match kind {
            ShellKind::Bash
                if shell.eq_ignore_ascii_case("bash") || shell.eq_ignore_ascii_case("bash.exe") =>
            {
                let mut roots = ["ProgramFiles", "ProgramFiles(x86)"]
                    .into_iter()
                    .filter_map(&env)
                    .map(|path| PathBuf::from(path).join("Git"))
                    .collect::<Vec<_>>();
                if let Some(local) = env("LOCALAPPDATA") {
                    roots.push(PathBuf::from(local).join("Programs/Git"));
                }
                if let Some(profile) = env("USERPROFILE") {
                    roots.push(PathBuf::from(profile).join("scoop/apps/git/current"));
                }
                for directory in &search {
                    if exists(&directory.join("git.exe"))
                        && let Some(parent) = directory.parent()
                    {
                        roots.push(parent.to_owned());
                    }
                }
                for root in roots {
                    candidates.push(root.join("bin/bash.exe"));
                    candidates.push(root.join("usr/bin/bash.exe"));
                }
            }
            ShellKind::PowerShell
                if shell.eq_ignore_ascii_case("pwsh") || shell.eq_ignore_ascii_case("pwsh.exe") =>
            {
                candidates.extend(
                    search
                        .iter()
                        .map(|directory| cwd.join(directory).join("powershell.exe")),
                );
                if let Some(system) = &system {
                    candidates.push(system.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
                }
            }
            _ => {}
        }
    }
    let mut unsupported = None;
    for candidate in candidates {
        if !exists(&candidate) {
            continue;
        }
        match checked(candidate) {
            Ok(path) => return Ok(path),
            Err(error) if error.code == "UnsupportedShellTransport" => unsupported = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(unsupported.unwrap_or_else(|| unavailable(shell)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn environment(key: &str) -> Option<OsString> {
        match key {
            "PATH" => {
                std::env::join_paths([Path::new("/windows/System32"), Path::new("/path")]).ok()
            }
            "SystemRoot" => Some("/windows".into()),
            "ProgramFiles" => Some("/Program Files".into()),
            "LOCALAPPDATA" => Some("/local".into()),
            _ => None,
        }
    }
    #[cfg(unix)]
    #[test]
    fn path_search_skips_nonexecutable_shadow_without_mutating_environment() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("eden-shell-path-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for directory in [&first, &second] {
            std::fs::write(directory.join("bash"), "fixture").unwrap();
        }
        std::fs::set_permissions(first.join("bash"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::set_permissions(second.join("bash"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let search = std::env::join_paths([first, second.clone()]).unwrap();
        assert_eq!(
            unix_shell("bash", &root, &search).unwrap(),
            second.join("bash")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_git_follows_path_and_skips_legacy_wsl() {
        let files = [
            Path::new("/windows/System32/bash.exe"),
            Path::new("/Program Files/Git/bin/bash.exe"),
        ];
        let result = windows_shell(
            "bash",
            ShellKind::Bash,
            Path::new("/cwd"),
            true,
            environment,
            |path| files.contains(&path),
        )
        .unwrap();
        assert_eq!(result, files[1]);
        let path = windows_shell(
            "bash",
            ShellKind::Bash,
            Path::new("/cwd"),
            true,
            environment,
            |path| path == Path::new("/path/bash.exe") || files.contains(&path),
        )
        .unwrap();
        assert_eq!(path, Path::new("/path/bash.exe"));
    }
    #[test]
    fn per_user_git_installation_is_found_without_path_entry() {
        let expected = Path::new("/local/Programs/Git/usr/bin/bash.exe");
        assert_eq!(
            windows_shell(
                "bash",
                ShellKind::Bash,
                Path::new("/cwd"),
                true,
                environment,
                |path| path == expected
            )
            .unwrap(),
            expected
        );
    }
    #[test]
    fn powershell_fallback_is_only_for_default_discovery() {
        let expected = Path::new("/windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        assert_eq!(
            windows_shell(
                "pwsh",
                ShellKind::PowerShell,
                Path::new("/cwd"),
                true,
                environment,
                |path| path == expected
            )
            .unwrap(),
            expected
        );
        assert!(
            windows_shell(
                "pwsh",
                ShellKind::PowerShell,
                Path::new("/cwd"),
                false,
                environment,
                |path| path == expected
            )
            .is_err()
        );
    }
    #[test]
    fn explicit_missing_and_wsl_paths_never_fall_back() {
        let missing = windows_shell(
            "/missing/bash.exe",
            ShellKind::Bash,
            Path::new("/cwd"),
            true,
            environment,
            |path| path == Path::new("/Program Files/Git/bin/bash.exe"),
        )
        .unwrap_err();
        assert_eq!(missing.code, "ShellUnavailable");
        let legacy = windows_shell(
            "/windows/System32/bash.exe",
            ShellKind::Bash,
            Path::new("/cwd"),
            true,
            environment,
            |_| true,
        )
        .unwrap_err();
        assert_eq!(legacy.code, "UnsupportedShellTransport");
    }
}

#[cfg(all(test, windows))]
mod native_windows_tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn default_powershell_fallback_launches_legacy_shell_in_unicode_cwd() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let cwd = std::env::temp_dir().join(format!(
            "eden-powershell5-space 目录-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let selected = windows_shell(
            "pwsh",
            ShellKind::PowerShell,
            &cwd,
            true,
            |key| {
                if key == "PATH" {
                    Some(OsString::new())
                } else {
                    std::env::var_os(key)
                }
            },
            |path| path.is_file(),
        )
        .unwrap();
        assert!(
            selected
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("powershell.exe")
        );
        let (mut child, tree) = crate::spawn(selected.to_str().unwrap(), &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'probe.txt'), 'native-powershell5'); [Console]::Out.Write('ok')"], &cwd).await.unwrap();
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut bytes)
            .await
            .unwrap();
        let status = child.wait().await.unwrap();
        tree.cleanup_descendants().unwrap();
        tree.settle().await.unwrap();
        assert!(status.success());
        assert_eq!(bytes, b"ok");
        assert_eq!(
            std::fs::read_to_string(cwd.join("probe.txt")).unwrap(),
            "native-powershell5"
        );
        std::fs::remove_dir_all(cwd).unwrap();
    }
}
