use super::*;
use std::sync::{Arc, Mutex};
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let n = stream.read(&mut buffer).await.unwrap();
        assert!(n > 0);
        data.extend_from_slice(&buffer[..n]);
        if let Some(end) = data.windows(4).position(|p| p == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]);
            let length = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                return String::from_utf8(data).unwrap();
            }
        }
    }
}
async fn serve(provider: &str) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let logged = requests.clone();
    let provider = provider.to_owned();
    let gateway = base.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap();
            let body = match path {
                "/discovery" => json!({ "authorization_endpoint": format!("{gateway}/authorize") }),
                "/device" => {
                    json!({
                        "device_code": "private-device-canary",
                        "device_auth_id": "private-device-canary",
                        "user_code": "public-code",
                        "verification_uri": format!("{gateway}/verify"),
                        "verification_uri_complete":
                            format!("{gateway}/verify?user_code=public-code"),
                        "interval": 1,
                        "expires_in": 600,
                    })
                }
                "/poll" => {
                    json!({
                        "authorization_code": "private-code-canary",
                        "code_verifier": "private-verifier-canary",
                    })
                }
                "/copilot" => {
                    json!({
                        "token": "copilot-access-canary",
                        "expires_at": now() + 3600,
                        "endpoints": { "api": gateway },
                    })
                }
                "/models" => {
                    json!({
                        "data": [{
                            "id": "gpt-4o",
                            "model_picker_enabled": true,
                            "policy": { "state": "enabled" },
                        }],
                    })
                }
                "/token" => {
                    let access = if provider == "openai-codex" {
                        format!(
                            "e30.{}.sig",
                            URL_SAFE_NO_PAD.encode(
                                json!({
                                    "https://api.openai.com/auth": {
                                            "chatgpt_account_id": "account-canary",
                                        },
                                })
                                .to_string()
                            )
                        )
                    } else {
                        "access-canary".into()
                    };
                    json!({
                        "key": "minted-key-canary",
                        "access_token": access,
                        "refresh_token": "rotated-refresh-canary",
                        "expires_in": 3600,
                    })
                }
                _ => panic!("unexpected fixture request: {path}"),
            };
            logged.lock().unwrap().push(request);
            let body = body.to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    (base, requests, task)
}
#[tokio::test]
async fn every_adapter_drives_its_exchange_refresh_and_private_request_material() {
    for (provider, method) in [
        ("anthropic", "browser"),
        ("openai-codex", "browser"),
        ("openai-codex", "device"),
        ("github-copilot", "device"),
        ("xai", "device"),
        ("kimi-coding", "device"),
        ("openrouter", "browser"),
        ("radius", "browser"),
        ("radius", "device"),
    ] {
        let (base, requests, server) = serve(provider).await;
        let config = Config {
            client_id: Some("authorized-eden-test".into()),
            authorization_url: Some(format!(
                "{base}/{}",
                if provider == "radius" {
                    "discovery"
                } else {
                    "authorize"
                }
            )),
            token_url: Some(format!("{base}/token")),
            device_url: Some(format!("{base}/device")),
            device_token_url: Some(format!("{base}/poll")),
            redirect_uri: Some("http://127.0.0.1:0/callback".into()),
            copilot_token_url: Some(format!("{base}/copilot")),
            ..Default::default()
        };
        let mut flow = Flow::start(provider, Some(method), &config).await.unwrap();
        let public = serde_json::to_string(&flow.interaction).unwrap();
        assert!(!public.contains("private-device-canary"));
        assert!(!public.contains(&flow.verifier));
        let token = if method == "device" {
            flow.wait().await.unwrap()
        } else if provider == "radius" {
            let callback = format!(
                "{}?code=authorization-canary&state={}",
                flow.settings.redirect_uri, flow.state
            );
            let response =
                tokio::spawn(async move { reqwest::get(callback).await.unwrap().status() });
            let token = flow.wait().await.unwrap();
            assert_eq!(response.await.unwrap(), 200);
            token
        } else {
            let input = if provider == "openrouter" {
                "authorization-canary".into()
            } else {
                format!("authorization-canary#{}", flow.state)
            };
            flow.submit(&input).await.unwrap()
        };
        let before = requests.lock().unwrap().len();
        let refreshed = token.refresh().await.unwrap();
        assert_eq!(token.catalog_scope, refreshed.catalog_scope);
        let private = refreshed.credential();
        assert!(private.api_key.is_some());
        if provider == "openai-codex" {
            assert_eq!(private.headers["ChatGPT-Account-Id"], "account-canary");
        }
        if matches!(provider, "anthropic" | "kimi-coding") {
            assert_eq!(private.headers["Authorization"], "Bearer access-canary");
        }
        if provider == "openrouter" {
            assert_eq!(requests.lock().unwrap().len(), before);
            assert_eq!(refreshed.expires_at, u64::MAX);
        } else if provider != "github-copilot" {
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .contains("rotated-refresh-canary")
            );
        } else {
            assert!(
                private
                    .available_model_ids
                    .as_ref()
                    .unwrap()
                    .contains(&"gpt-4o".into())
            );
            assert_eq!(private.base_url.as_deref(), Some(base.as_str()));
        }
        let all = requests.lock().unwrap().join("\n");
        if provider == "anthropic" || provider == "openrouter" {
            assert!(all.contains("application/json"));
        } else if provider != "openai-codex" || method != "device" {
            assert!(all.contains("application/x-www-form-urlencoded"));
        }
        server.abort();
        let _ = server.await;
    }
}
#[tokio::test]
async fn expired_manual_login_cannot_exchange_a_code() {
    let (base, _, server) = serve("openrouter").await;
    let config = Config {
        token_url: Some(format!("{base}/token")),
        ..Default::default()
    };
    let mut flow = Flow::start("openrouter", None, &config).await.unwrap();
    flow.interaction.expires_at = now() - 1;
    let error = match flow.submit("code-canary").await {
        Err(error) => error,
        Ok(_) => panic!("expired login exchanged"),
    };
    server.abort();
    let _ = server.await;
    assert!(error.message.contains("expired"));
}
#[tokio::test]
async fn denied_exchange_redacts_response_canaries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        token_url: Some(format!("http://{}/token", listener.local_addr().unwrap())),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        let body = json!({ "error": "invalid_grant", "message": "secret-canary" }).to_string();
        stream.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let flow = Flow::start("openrouter", None, &config).await.unwrap();
    let error = match flow.submit("private-code-canary").await {
        Err(e) => e,
        Ok(_) => panic!("unexpected success"),
    };
    assert!(!serde_json::to_string(&error).unwrap().contains("canary"));
    assert!(error.message.contains("401"));
    server.await.unwrap();
}
