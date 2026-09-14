use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Project(std::path::PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "eden-env-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("child")).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_eden"));
        cmd.current_dir(self.0.join("child"));
        cmd
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn explicit_env_file_is_loaded_without_searching_parent_files() {
    let p = Project::new();
    fs::write(p.0.join(".env"), "invalid dotenv parent\n").unwrap();
    let ordinary = p.command().arg("--version").output().unwrap();
    assert!(ordinary.status.success());
    fs::write(
        p.0.join("selected.env"),
        "LOCAL_VALUE='literal $(do not execute)'\n",
    )
    .unwrap();
    let explicit = p
        .command()
        .args(["--env-file", "../selected.env", "--version"])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("eden"));
}
#[test]
fn invalid_explicit_file_fails_without_echoing_credentials() {
    let p = Project::new();
    let secret = "fixture-credential-not-for-diagnostics";
    fs::write(
        p.0.join("bad.env"),
        format!("KEY={secret}\nINVALID {secret}\n"),
    )
    .unwrap();
    let result = p
        .command()
        .args(["--env-file", "../bad.env", "--version"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(diagnostic.contains("env file"), "{diagnostic}");
    assert!(!diagnostic.contains(secret));
}
#[test]
fn missing_or_duplicate_explicit_file_is_rejected() {
    let p = Project::new();
    for args in [
        vec!["--env-file"],
        vec!["--env-file", "missing.env", "--version"],
        vec!["--env-file", "a", "--env-file", "b", "--version"],
    ] {
        assert!(!p.command().args(args).output().unwrap().status.success());
    }
}
