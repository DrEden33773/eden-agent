use super::*;

#[tokio::test]
async fn older_pi_cache_cannot_override_a_newer_bundled_model() {
    let catalog = Catalog {
        refresh_lock: Mutex::new(()),
        background_job: Mutex::new(None),
        config: Config::default(),
        inner: Mutex::new(Inner {
            source: "https://pi.dev".into(),
            generation: 0,
            disk: Disk::default(),
        }),
    };
    let mut inner = catalog.inner.lock().await;
    let original = catalog
        .entries(&inner)
        .unwrap()
        .into_iter()
        .find(|e| e.target.provider == "openai" && e.target.model == "gpt-4.1")
        .unwrap();
    let stale: Cached = serde_json::from_value(serde_json::json!({
        "models": [{
            "id": "gpt-4.1",
            "api": "openai-responses",
            "baseUrl": "https://old.example/v1",
            "contextWindow": 111,
        }],
        "etag": "old",
        "updated_at": 9999999999_u64,
        "last_modified": 1,
    }))
    .unwrap();
    inner.disk.sources.insert(
        "https://pi.dev".into(),
        BTreeMap::from([("openai".into(), stale)]),
    );
    let actual = catalog
        .entries(&inner)
        .unwrap()
        .into_iter()
        .find(|e| e.target.provider == "openai" && e.target.model == "gpt-4.1")
        .unwrap();
    assert_eq!(actual.target.base_url, original.target.base_url);
    assert_eq!(
        actual.target.limits.context_window,
        original.target.limits.context_window
    );
}

#[test]
fn bundled_release_contains_new_providers_and_copilot_responses_route() {
    let bundled: Value = serde_json::from_str(BUNDLED).unwrap();
    assert!(bundled["meta"].is_object());
    assert!(bundled["radius"].is_object());
    assert_eq!(
        bundled["github-copilot"]["openai-responses"]["gpt-6-astra"]["api"],
        "openai-responses"
    );
    let model = target(
        "github-copilot",
        &bundled["github-copilot"]["openai-responses"]["gpt-6-astra"],
        CatalogSource::default(),
    )
    .unwrap();
    let input = serde_json::from_value(serde_json::json!({ "items": [], "tools": [] })).unwrap();
    let request = crate::targeted::prepare(&model, &input).unwrap();
    assert!(request.body["input"].is_array());
    assert!(request.body.get("messages").is_none());
    assert_eq!(
        crate::subscription::headers(&model, &input)["Openai-Intent"],
        "conversation-edits"
    );
    let source: Value = serde_json::from_str(BUNDLED_SOURCE).unwrap();
    assert_eq!(source["package"], "@earendil-works/pi-ai@0.87.1");
    assert_eq!(source["generated_at_iso"], "2026-09-22T19:31:44.346Z");
    assert_eq!(bundled_generated_at(), 1790105504);
}

#[tokio::test]
async fn fresh_catalog_skips_network_but_manual_refresh_revalidates() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = format!("http://{}", listener.local_addr().unwrap());
    let catalog = Catalog {
        refresh_lock: Mutex::new(()),
        background_job: Mutex::new(None),
        config: Config::default(),
        inner: Mutex::new(Inner {
            source: source.clone(),
            generation: 0,
            disk: Disk::default(),
        }),
    };
    let providers: BTreeMap<String, Value> = serde_json::from_str(BUNDLED).unwrap();
    let cached = providers
        .keys()
        .filter(|p| *p != "radius")
        .map(|p| {
            (
                p.clone(),
                serde_json::from_value(serde_json::json!({
                    "models": [{
                        "id": "remote",
                        "api": "openai-responses",
                        "baseUrl": "http://localhost",
                    }],
                    "etag": "fresh",
                    "updated_at": now(),
                }))
                .unwrap(),
            )
        })
        .collect();
    catalog
        .inner
        .lock()
        .await
        .disk
        .sources
        .insert(source, cached);
    assert_eq!(catalog.refresh_public(None, false).await.unwrap(), "fresh");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    let count = providers.len() - 1;
    let server = tokio::spawn(async move {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            let read = socket.read(&mut bytes).await.unwrap();
            assert!(
                String::from_utf8_lossy(&bytes[..read])
                    .to_ascii_lowercase()
                    .contains("if-none-match: fresh")
            );
            socket
                .write_all(
                    b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    assert_eq!(catalog.refresh(None).await.unwrap(), "refreshed");
    server.await.unwrap();
}

#[test]
fn bundled_gemini_medium_reaches_the_native_wire() {
    let bundled: Value = serde_json::from_str(BUNDLED).unwrap();
    let record = &bundled["google"]["google-generative-ai"]["gemini-3.1-pro-preview"];
    let mut model = target("google", record, CatalogSource::default()).unwrap();
    model.thinking.effective = effective_thinking(&model, Some("medium"));
    let input = serde_json::from_value(serde_json::json!({ "items": [], "tools": [] })).unwrap();
    assert_eq!(
        crate::gemini::project(&input, &model).unwrap()["generationConfig"]["thinkingConfig"]
            ["thinkingLevel"],
        "MEDIUM"
    );
}

#[tokio::test]
async fn cache_lock_wait_yields_to_the_refresh_deadline() {
    use std::time::Duration;
    let path =
        std::env::temp_dir().join(format!("eden-catalog-deadline-{}.json", std::process::id()));
    let lock_path = path.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let owner = std::thread::spawn(move || {
        let _ = released.recv_timeout(Duration::from_secs(1));
        drop(lock);
    });
    let catalog = Catalog {
        refresh_lock: Mutex::new(()),
        background_job: Mutex::new(None),
        config: Config {
            cache_path: Some(path.clone()),
            ..Default::default()
        },
        inner: Mutex::new(Inner {
            source: "https://pi.dev".into(),
            generation: 0,
            disk: Disk::default(),
        }),
    };
    let result = tokio::time::timeout(Duration::from_millis(50), async {
        let mut inner = catalog.inner.lock().await;
        catalog.persist(&mut inner, Persist::Default).await
    })
    .await;
    let _ = release.send(());
    owner.join().unwrap();
    let written = path.exists();
    let _ = std::fs::remove_file(path);
    std::fs::remove_file(lock_path).unwrap();
    assert!(
        result.is_err(),
        "a contended filesystem lock blocked the async deadline"
    );
    assert!(!written, "cancelled persistence wrote a catalog");
}

#[tokio::test]
async fn refresh_deadline_keeps_providers_that_already_succeeded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut handlers = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            handlers.spawn(async move {
                let mut request = Vec::new();
                let mut bytes = [0;2048];
                while !request.windows(4).any(|p| p == b"\r\n\r\n") {
                    let n = socket.read(&mut bytes).await.unwrap();
                    if n == 0 { return; }
                    request.extend_from_slice(&bytes[..n]);
                }
                if String::from_utf8_lossy(&request).contains("/api/models/providers/amazon-bedrock ") {
                    let body = r#"[{"id":"fast-model","api":"openai-responses","baseUrl":"http://localhost/v1"}]"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                } else {
                    // Hold the other requests until the client deadline drops its connections.
                    let _ = socket.read(&mut bytes).await;
                }
            });
        }
    });
    let path =
        std::env::temp_dir().join(format!("eden-partial-catalog-{}.json", std::process::id()));
    let catalog = Catalog {
        refresh_lock: Mutex::new(()),
        background_job: Mutex::new(None),
        config: Config {
            cache_path: Some(path.clone()),
            ..Default::default()
        },
        inner: Mutex::new(Inner {
            source: source.clone(),
            generation: 0,
            disk: Disk::default(),
        }),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        catalog.refresh_public(None, false),
    )
    .await;
    server.abort();
    let disk = std::fs::read(&path)
        .ok()
        .map(|b| serde_json::from_slice::<Value>(&b).unwrap());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("lock"));
    assert!(
        result.is_err(),
        "slow providers should remain pending at the deadline"
    );
    assert_eq!(
        disk.unwrap_or_default()["sources"][&source]["amazon-bedrock"]["models"][0]["id"],
        "fast-model"
    );
}
