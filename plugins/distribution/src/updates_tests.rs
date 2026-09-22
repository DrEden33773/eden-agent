use super::*;

#[test]
fn stable_never_selects_prerelease_and_tag_is_exact() {
    let releases = serde_json::json!([
        { "tag_name": "v2.0.0-rc1", "prerelease": true, "draft": false },
        { "tag_name": "v1.0.0", "prerelease": false, "draft": false }
    ]);
    assert_eq!(
        select_release(&releases, &Channel::Stable).unwrap()["tag_name"],
        "v1.0.0"
    );
    assert_eq!(
        select_release(&releases, &Channel::Prerelease).unwrap()["tag_name"],
        "v2.0.0-rc1"
    );
    assert!(
        select_release(
            &releases,
            &Channel::Tag {
                tag: "absent".into()
            }
        )
        .is_none()
    );
}

#[test]
fn activation_is_append_only_and_lock_excludes_competitors() {
    let root = std::env::temp_dir().join(format!("eden-update-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lock = OperationLock::acquire(&root).unwrap();
    assert!(OperationLock::acquire(&root).is_err());
    drop(lock);
    let _next = OperationLock::acquire(&root).unwrap();
    drop(_next);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn manifest_rejects_missing_and_unlisted_files() {
    let root = std::env::temp_dir().join(format!("eden-update-manifest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("bin"), b"host").unwrap();
    let mut manifest = ReleaseManifest {
        version: "1.0.0".into(),
        target: eden_plugin_sdk::abi::TARGET.into(),
        executable: "bin".into(),
        files: std::collections::BTreeMap::new(),
    };
    manifest
        .files
        .insert("bin".into(), format!("{:x}", Sha256::digest(b"host")));
    verify_files(&root, &manifest, &Cancellation::default()).unwrap();
    std::fs::write(root.join("extra"), "unexpected").unwrap();
    assert!(verify_files(&root, &manifest, &Cancellation::default()).is_err());
    std::fs::remove_file(root.join("extra")).unwrap();
    std::fs::write(root.join("bin"), "changed").unwrap();
    assert!(verify_files(&root, &manifest, &Cancellation::default()).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "eden-update-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn updater(&self) -> Updater {
        Updater {
            manager: Arc::new(Manager {
                root: self.0.join("packages"),
                client: source::client(None).unwrap(),
            }),
            config: Config {
                managed_root: Some(self.0.join("managed")),
                sources: vec![],
            },
        }
    }
    fn candidate(&self, version: &str, valid: bool) -> Candidate {
        let path = self.0.join(version);
        std::fs::create_dir_all(path.join("bin")).unwrap();
        let executable = if cfg!(windows) {
            "bin/eden.exe"
        } else {
            "bin/eden"
        };
        if valid {
            std::fs::copy(std::env::current_exe().unwrap(), path.join(executable)).unwrap();
        } else {
            std::fs::write(path.join(executable), "not an executable").unwrap();
        }
        std::fs::write(path.join("package.json"), "{}").unwrap();
        let mut files = BTreeMap::new();
        for name in [executable, "package.json"] {
            files.insert(
                name.into(),
                format!(
                    "{:x}",
                    Sha256::digest(std::fs::read(path.join(name)).unwrap())
                ),
            );
        }
        let manifest = ReleaseManifest {
            version: version.into(),
            target: eden_plugin_sdk::abi::TARGET.into(),
            executable: executable.into(),
            files,
        };
        std::fs::write(
            path.join("release.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        Candidate {
            target: UpdateTarget::Host,
            version: version.into(),
            source: json!({ "kind": "local", "path": path }),
            channel: Channel::Stable,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn prepare_is_inert_activation_retains_old_installation_and_bad_start_preserves_pointer() {
    let fixture = Fixture::new();
    let updater = fixture.updater();
    let root = updater.config.managed_root.as_ref().unwrap();
    let first = updater
        .prepare(fixture.candidate("v1", true), Cancellation::default())
        .await
        .unwrap();
    let UpdateReply::Prepared { prepared: first } = first else {
        panic!("expected preparation")
    };
    assert!(active_installation(root).unwrap().is_none());
    updater
        .activate(first.clone(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(
        active_installation(root).unwrap(),
        Some(PathBuf::from(&first.path))
    );
    let failure = updater
        .prepare(
            fixture.candidate("v2-broken", false),
            Cancellation::default(),
        )
        .await;
    assert!(failure.is_err());
    assert_eq!(
        active_installation(root).unwrap(),
        Some(PathBuf::from(&first.path))
    );
    let UpdateReply::Prepared { prepared: second } = updater
        .prepare(fixture.candidate("v2", true), Cancellation::default())
        .await
        .unwrap()
    else {
        panic!("expected preparation")
    };
    updater
        .activate(second.clone(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(
        active_installation(root).unwrap(),
        Some(PathBuf::from(&second.path))
    );
    assert!(Path::new(&first.path).join("package.json").is_file());
    std::fs::write(Path::new(&first.path).join("package.json"), "tampered").unwrap();
    assert!(
        updater
            .activate(first, &Cancellation::default())
            .await
            .is_err()
    );
    assert_eq!(
        active_installation(root).unwrap(),
        Some(PathBuf::from(second.path))
    );
}

#[tokio::test]
async fn unconfigured_plugin_and_manual_host_never_mutate_installation() {
    let fixture = Fixture::new();
    let mut updater = fixture.updater();
    updater.config.managed_root = None;
    let reply = updater
        .dispatch(
            UpdateRequest::Check {
                target: UpdateTarget::Plugin {
                    name: "local".into(),
                },
                channel: Channel::Stable,
            },
            Cancellation::default(),
        )
        .await
        .unwrap();
    let UpdateReply::Checked { status } = reply else {
        panic!("expected checked")
    };
    assert!(!status.configured);
    assert!(status.candidate.is_none());
    assert_eq!(
        updater
            .prepare(fixture.candidate("v1", true), Cancellation::default())
            .await
            .unwrap_err()
            .code,
        "ManualUpdateRequired"
    );
    assert!(!fixture.0.join("managed").exists());
}

#[test]
fn lock_child_probe() {
    if let Some(path) = std::env::var_os("EDEN_TEST_UPDATE_LOCK") {
        assert!(OperationLock::acquire(Path::new(&path)).is_err());
    }
}

#[test]
fn update_lock_excludes_another_process() {
    let fixture = Fixture::new();
    let _lock = OperationLock::acquire(&fixture.0).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "updates::tests::lock_child_probe"])
        .env("EDEN_TEST_UPDATE_LOCK", &fixture.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn cancellation_before_commit_keeps_old_activation() {
    let fixture = Fixture::new();
    let updater = fixture.updater();
    let UpdateReply::Prepared { prepared } = updater
        .prepare(fixture.candidate("v1", true), Cancellation::default())
        .await
        .unwrap()
    else {
        panic!("expected prepared")
    };
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(updater.activate(prepared, &cancelled).await.is_err());
    assert!(
        active_installation(updater.config.managed_root.as_ref().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn latest_activation_fails_closed_and_ignores_pending_record() {
    let fixture = Fixture::new();
    let records = fixture.0.join("activations");
    std::fs::create_dir(&records).unwrap();
    std::fs::write(records.join("pending.json"), "partial").unwrap();
    assert!(active_installation(&fixture.0).unwrap().is_none());
    std::fs::write(
        records.join("00000000000000000001.json"),
        r#"{"path":"../outside"}"#,
    )
    .unwrap();
    assert!(active_installation(&fixture.0).is_err());
    std::fs::write(
        records.join("00000000000000000001.json"),
        r#"{"path":"releases/missing"}"#,
    )
    .unwrap();
    assert!(active_installation(&fixture.0).is_err());
}
