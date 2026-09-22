//! Explicit secret-gist publication of previously prepared bytes, without retries.
use eden_plugin_sdk::{
    Cancellation, Package,
    protocol::{Descriptor, Fault, delivery::*},
    serde_json::{Value, json},
};
use std::sync::Arc;
struct Publisher {
    client: reqwest::Client,
    endpoint: String,
    token_env: String,
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "github-share", message)
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "github-share".into(),
        version: "0.1.0".into(),
        provides: vec![SHARE_TARGET.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let endpoint = config["endpoint"]
        .as_str()
        .unwrap_or("https://api.github.com/gists")
        .to_owned();
    let url = reqwest::Url::parse(&endpoint).map_err(|e| fault("InvalidInput", e.to_string()))?;
    if !(url.scheme() == "https"
        || url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]")))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(fault(
            "InvalidInput",
            "share endpoint requires HTTPS or explicit loopback HTTP without URL credentials",
        ));
    }
    let publisher = Arc::new(Publisher {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| fault("Unavailable", e.to_string()))?,
        endpoint,
        token_env: config["token_env"].as_str().unwrap_or("GH_TOKEN").into(),
    });
    Ok(
        Package::new("github-share").service(SHARE_TARGET, move |request: PublishRequest, cx| {
            let publisher = publisher.clone();
            async move {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let cancel = cx.scope.cancellation();
                let scope = cx.scope.clone();
                cx.scope.spawn(async move {
                    if request.confirmed {
                        scope.retain_result(json!({
                            "publication": "unknown",
                            "notice":
                                "Remote creation may have occurred; check your gists before \
                                 retrying.",
                        }))?;
                    }
                    let result = publisher.publish(request, cancel).await;
                    if let Ok(reply) = &result {
                        scope.retain_result(json!(reply))?;
                    }
                    let _ = tx.send(result);
                    Ok(())
                })?;
                rx.await.map_err(|_| {
                    fault(
                        "PublicationUnknown",
                        "publication completion lost; check your gists before retrying",
                    )
                })?
            }
        }),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);
impl Publisher {
    async fn publish(
        &self,
        request: PublishRequest,
        cancel: Cancellation,
    ) -> Result<PublishReply, Fault> {
        if !request.confirmed {
            return Err(fault(
                "ConfirmationRequired",
                "preview the prepared artifact and explicitly confirm publication",
            ));
        }
        if cancel.is_cancelled() {
            return Err(fault("Cancelled", "publication cancelled before dispatch"));
        }
        if !matches!(
            request.artifact.filename.as_str(),
            "conversation.html" | "conversation.jsonl"
        ) {
            return Err(fault("InvalidInput", "unsupported artifact filename"));
        }
        let token = std::env::var(&self.token_env)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                fault(
                    "CredentialRequired",
                    format!(
                        "set {} to a GitHub token with gist permission",
                        self.token_env
                    ),
                )
            })?;
        let body = json!({
            "description": "Eden conversation reading copy",
            "public": false,
            "files": { request.artifact.filename: { "content": request.artifact.content } },
        });
        let send = async {
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(token)
                .header("User-Agent", "eden-agent")
                .header("Accept", "application/vnd.github+json")
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|_| {
                    fault(
                        "PublicationUnknown",
                        "request outcome unknown; check your gists before retrying",
                    )
                })?;
            if matches!(response.status().as_u16(), 401 | 403) {
                return Err(fault(
                    "CredentialRejected",
                    "GitHub rejected the gist credential or permission",
                ));
            }
            if !response.status().is_success() {
                return Err(fault(
                    "PublicationUnknown",
                    format!(
                        "publisher returned {}; check your gists before retrying",
                        response.status()
                    ),
                ));
            }
            let bytes = response.bytes().await.map_err(|_| {
                fault(
                    "PublicationUnknown",
                    "publication response lost; check your gists before retrying",
                )
            })?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                fault(
                    "PublicationUnknown",
                    "invalid publication response; check your gists before retrying",
                )
            })?;
            let url = value["html_url"].as_str().ok_or_else(|| {
                fault(
                    "PublicationUnknown",
                    "publication response missing URL; check your gists before retrying",
                )
            })?;
            Ok(PublishReply {
                url: url.into(),
                visibility: "secret".into(),
                notice: "Anyone with the URL can access this secret gist. Download the HTML and \
                         open it locally; no hosted viewer is required."
                    .into(),
            })
        };
        tokio::select! {
            result = send => result,
            _ = cancel.cancelled() => Err(fault(
                "PublicationUnknown",
                "publication cancelled after dispatch; remote creation may have occurred; check \
                 your gists before retrying"
            )),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn no_confirmation_never_needs_credentials_or_connects() {
        let p = Publisher {
            client: reqwest::Client::new(),
            endpoint: "http://127.0.0.1:1".into(),
            token_env: "EDEN_TEST_UNUSED_TOKEN".into(),
        };
        let error = p
            .publish(
                PublishRequest {
                    confirmed: false,
                    artifact: Artifact {
                        media_type: "text/html".into(),
                        filename: "conversation.html".into(),
                        content: "private".into(),
                        warnings: vec![],
                    },
                },
                Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "ConfirmationRequired");
    }
}
