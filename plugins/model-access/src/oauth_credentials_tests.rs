//! AUTH03/AUTH04 exercise durable credentials and real cross-process locking.
use super::*;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
fn state(path: Option<PathBuf>) -> Credentials {
    let mut state = super::tests::credentials();
    if let Some(path) = path {
        state.config.path = Some(path);
    }
    state
}
fn token(endpoint: &str, expires_at: u64) -> oauth::Token {
    serde_json::from_value(json!({
        "settings": {
            "provider": "xai",
            "client_id": "admitted-test-client",
            "authorization_url": "",
            "token_url": endpoint,
            "device_url": "",
            "device_token_url": "",
            "redirect_uri": "",
            "scope": "",
            "copilot_token_url": "",
            "domain": "github.com",
            "headers": {},
        },
        "access": "old-access-canary",
        "refresh": "old-refresh-canary",
        "expires_at": expires_at,
        "base_url": null,
        "account_id": null,
        "catalog_scope": "account-one",
        "available_model_ids": null,
    }))
    .unwrap()
}
async fn request(stream: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut length = None;
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        data.extend_from_slice(&chunk[..count]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]);
            let n = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            length = Some(end + 4 + n);
        }
        if length.is_some_and(|n| data.len() >= n) {
            break;
        }
    }
    String::from_utf8(data).unwrap()
}
async fn reply(stream: &mut tokio::net::TcpStream, value: Value) {
    let body = value.to_string();
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
}
#[tokio::test]
async fn rotation_persists_and_late_refresh_cannot_resurrect_logout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let state = Arc::new(state(None));
    state
        .store(
            |s| {
                s.oauth.insert("xai".into(), token(&endpoint, 0));
                s.keys.insert("other".into(), "other-key".into());
                Ok(())
            },
            true,
        )
        .unwrap();
    let refresh = {
        let state = state.clone();
        tokio::spawn(async move { state.oauth_token("xai", false, "inference").await })
    };
    let (mut stream, _) = listener.accept().await.unwrap();
    assert!(
        request(&mut stream)
            .await
            .contains("refresh_token=old-refresh-canary")
    );
    reply(
        &mut stream,
        json!({
            "access_token": "new-access-canary",
            "refresh_token": "rotated-refresh-canary",
            "expires_in": 3600,
        }),
    )
    .await;
    refresh.await.unwrap().unwrap();
    let disk = std::fs::read_to_string(state.config.path.as_ref().unwrap()).unwrap();
    assert!(disk.contains("rotated-refresh-canary"));
    assert!(!disk.contains("old-refresh-canary"));
    assert!(disk.contains("other-key"));
    let reopened = Arc::new(super::oauth_tests::state(state.config.path.clone()));
    let refresh = {
        let reopened = reopened.clone();
        tokio::spawn(async move { reopened.oauth_token("xai", true, "refresh").await })
    };
    let (mut stream, _) = listener.accept().await.unwrap();
    assert!(
        request(&mut stream)
            .await
            .contains("refresh_token=rotated-refresh-canary")
    );
    state
        .auth(
            AuthRequest::Logout {
                provider: "xai".into(),
            },
            None,
        )
        .await
        .unwrap();
    reply(
        &mut stream,
        json!({
            "access_token": "late-secret",
            "refresh_token": "late-refresh",
            "expires_in": 3600,
        }),
    )
    .await;
    assert!(refresh.await.unwrap().is_err());
    assert!(
        !std::fs::read_to_string(state.config.path.as_ref().unwrap())
            .unwrap()
            .contains("late-secret")
    );
    assert!(
        state
            .oauth_token("xai", false, "inference")
            .await
            .unwrap()
            .is_none()
    );
    std::fs::remove_dir_all(state.config.path.as_ref().unwrap().parent().unwrap()).unwrap();
}
#[test]
fn refresh_child() {
    let Ok(path) = std::env::var("EDEN_G3_REFRESH_PATH") else {
        return;
    };
    let state = state(Some(path.into()));
    writeln!(std::io::stdout(), "READY").unwrap();
    std::io::stdout().flush().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let credential = runtime
        .block_on(state.resolve(
            CredentialRequest {
                provider: "xai".into(),
                explicit: None,
                purpose: "inference".into(),
            },
            None,
        ))
        .unwrap();
    assert_eq!(credential.api_key.as_deref(), Some("rotated-access"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_share_one_rotating_refresh_grant() {
    use std::io::{BufRead, BufReader};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let state = state(None);
    state
        .store(
            |s| {
                s.oauth.insert("xai".into(), token(&endpoint, 0));
                Ok(())
            },
            true,
        )
        .unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "credentials::oauth_tests::refresh_child",
                "--nocapture",
            ])
            .env("EDEN_G3_REFRESH_PATH", state.config.path.as_ref().unwrap())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert!(output.read_line(&mut line).unwrap() > 0);
            if line.contains("READY") {
                break;
            }
            line.clear();
        }
        children.push((child, output));
    }
    let (mut stream, _) = listener.accept().await.unwrap();
    assert!(request(&mut stream).await.contains("old-refresh-canary"));
    // Closing the listener makes a duplicate exchange fail rather than be silently tolerated.
    drop(listener);
    reply(
        &mut stream,
        json!({
            "access_token": "rotated-access",
            "refresh_token": "rotated-refresh",
            "expires_in": 3600,
        }),
    )
    .await;
    for (mut child, mut output) in children {
        std::io::copy(&mut output, &mut std::io::sink()).unwrap();
        assert!(child.wait().unwrap().success());
    }
    assert!(
        std::fs::read_to_string(state.config.path.as_ref().unwrap())
            .unwrap()
            .contains("rotated-refresh")
    );
    std::fs::remove_dir_all(state.config.path.as_ref().unwrap().parent().unwrap()).unwrap();
}
#[tokio::test]
async fn cancelled_refresh_closes_socket_and_releases_cross_process_lock() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let state = Arc::new(state(None));
    state
        .store(
            |s| {
                s.oauth.insert("xai".into(), token(&endpoint, 0));
                Ok(())
            },
            true,
        )
        .unwrap();
    let refresh = {
        let state = state.clone();
        tokio::spawn(async move { state.oauth_token("xai", false, "inference").await })
    };
    let (mut stream, _) = listener.accept().await.unwrap();
    request(&mut stream).await;
    refresh.abort();
    assert!(matches!(refresh.await, Err(error) if error.is_cancelled()));
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let next = {
        let state = state.clone();
        tokio::spawn(async move { state.oauth_token("xai", false, "inference").await })
    };
    let (mut stream, _) = listener.accept().await.unwrap();
    request(&mut stream).await;
    reply(
        &mut stream,
        json!({ "access_token": "usable", "expires_in": 3600 }),
    )
    .await;
    assert_eq!(
        next.await
            .unwrap()
            .unwrap()
            .unwrap()
            .credential()
            .api_key
            .as_deref(),
        Some("usable")
    );
    assert!(
        std::fs::read_to_string(state.config.path.as_ref().unwrap())
            .unwrap()
            .contains("old-refresh-canary")
    );
    std::fs::remove_dir_all(state.config.path.as_ref().unwrap().parent().unwrap()).unwrap();
}
#[tokio::test]
async fn logout_invalidates_another_instances_pending_login_and_cancels_owned_listener() {
    let state = state(None);
    let opened = state
        .auth(
            AuthRequest::Login {
                provider: "openrouter".into(),
                method: None,
            },
            None,
        )
        .await
        .unwrap();
    let id = opened.operation_id.unwrap();
    let op = state.oauth_operation(&id).await.unwrap();
    let url = reqwest::Url::parse(&opened.interaction.unwrap().url).unwrap();
    let callback = url
        .query_pairs()
        .find(|(k, _)| k == "callback_url")
        .unwrap()
        .1
        .into_owned();
    let callback = reqwest::Url::parse(&callback).unwrap();
    state
        .auth(
            AuthRequest::Logout {
                provider: "openrouter".into(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(op.reply().status, "cancelled");
    assert!(
        TcpListener::bind(("127.0.0.1", callback.port().unwrap()))
            .await
            .is_ok()
    );
    assert!(
        state
            .auth(
                AuthRequest::Submit {
                    operation_id: id,
                    input: "code-canary".into()
                },
                None
            )
            .await
            .is_err()
    );
    std::fs::remove_dir_all(state.config.path.as_ref().unwrap().parent().unwrap()).unwrap();
}
