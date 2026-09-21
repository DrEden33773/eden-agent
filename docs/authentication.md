# Account login

The `model-access` package provides browser login, device authorization and OpenRouter key creation through the same private authentication contract used by the CLI and Rust SDK. Login operations live in one open session. Credentials are stored separately from conversation history; operation IDs cannot resume an interrupted login after the process exits.

## Configure an authorized client

Set `plugins.model-access.credentials.oauth.PROVIDER.client_id` in the user `settings.json` to a client authorized for the provider and intended application. OpenRouter key creation does not require this setting. An absent client ID fails explicitly; Eden does not reuse another application's registered identity. Account eligibility, client admission and billing remain provider-specific.

```json
{
  "plugins": {
    "model-access": {
      "credentials": {
        "oauth": {
          "openai-codex": {
            "client_id": "YOUR_AUTHORIZED_CLIENT_ID"
          }
        }
      }
    }
  }
}
```

| Provider ID | Login methods | Default |
| --- | --- | --- |
| `anthropic` | Browser | Browser |
| `openai-codex` | Browser, device | Browser |
| `github-copilot` | Device | Device |
| `xai` | Device | Device |
| `kimi-coding` | Device | Device |
| `radius` | Browser, device | Browser |
| `openrouter` | Browser key creation | Browser |

### Endpoint configuration

Each OAuth entry also accepts `authorization_url`, `token_url`, `device_url`, `device_token_url`, `redirect_uri`, `scope`, and `headers` for an admitted endpoint or integration. `domain` selects a GitHub Enterprise host, `copilot_token_url` overrides its subscription exchange, and Radius `gateway` supplies discovery/device/token endpoints. HTTPS is required except for local loopback fixtures. Callback redirects must use IPv4 loopback HTTP; a busy port reports an error. Browser defaults are port 53692 for Anthropic, 1455 for Codex, 1456 for Radius, and a random port/path for OpenRouter.

The private stored credential retains its issuing endpoint and client identity. Editing configuration does not silently send an existing refresh token to a new issuer; log in again to change that binding. Rotating refresh tokens are atomically persisted under an OS lock shared across processes. Logout invalidates in-flight publication, so a delayed exchange cannot restore the logged-out account. Local logout does not revoke provider-side authorization or delete an OpenRouter key at the service; manage remote grants in the provider's account settings.

Claude account login targets admitted third-party extra usage, not a promise of Pro/Max included usage. Codex, Copilot, xAI, Kimi and Radius access likewise requires provider acceptance of the client and account. API-key access remains independent.

## CLI

Use the same installed composition and global configuration directory as for model commands.

```sh
eden auth login openai-codex
eden auth login openai-codex --method device
eden auth login github-copilot
eden auth login openrouter
eden auth refresh openai-codex
eden auth logout openai-codex
```

The command prints an authorization URL and, for device authorization, a user code to stderr. Open the URL in your browser and complete the provider's instructions. When manual input is supported, paste the returned code or redirect URL into stdin and press Enter; EOF is not required. Radius completes through its browser callback or device flow and does not accept manual input. Authorization codes and credentials are never command-line arguments.

The CLI waits for completion and writes the redacted final reply as JSON to stdout. Ctrl-C cancels the active authentication run and waits for cleanup before returning a failure exit status. Browser completion also closes and reaps a pending stdin reader. A fresh invocation starts a new login instead of recovering an old operation ID.

`eden auth set PROVIDER < /private/key-file` remains available for API keys. Logout removes managed credentials; an environment key or other external source can remain configured. Refresh uses the managed account and reports failure without silently switching to another credential source. OpenRouter login creates an API key rather than a refreshable OAuth grant.

## Rust SDK

`Session::authenticate` starts a managed run and returns its run ID. Wait for its terminal before starting another managed action. `Login` returns an `AuthReply` with an operation ID and optional `AuthInteraction`; display the interaction in transient UI, then start `Wait` on the same session.

```rust
use eden_agent::Session;
use eden_protocol::models::{AuthReply, AuthRequest};

async fn begin_login(session: &Session) -> Result<(String, u64), Box<dyn std::error::Error>> {
    let start = session.authenticate(AuthRequest::Login {
        provider: "openai-codex".into(),
        method: Some("browser".into()),
    })?;
    let reply: AuthReply = serde_json::from_value(session.wait(start).await?.into_result()?)?;
    if let Some(interaction) = reply.interaction {
        // Display URL/user_code and enable private input only when manual_input is true.
        show_login(interaction);
    }
    let operation_id = reply.operation_id.ok_or("missing login operation")?;
    let wait = session.authenticate(AuthRequest::Wait {
        operation_id: operation_id.clone(),
    })?;
    Ok((operation_id, wait))
}
```

`show_login` represents the application's own UI. The service owns PKCE, state checking, callback sockets, expiration, device polling and token exchange. The UI only displays the challenge and optionally supplies private input.

While `Wait` is active, `session.auth_status(&operation_id).await` reads its redacted status. `session.submit_auth_input(&operation_id, private_input).await` queues a pasted code or redirect without starting another managed run. Keep `Wait` running: submission acknowledgment does not mean credentials were stored. Await `session.wait(wait).await?.into_result()?` for the final outcome. Do not log the input, persist the interaction or copy either into conversation records.

To cancel an active wait, call `session.cancel(wait)?` and await `session.wait(wait).await?` as the cleanup barrier. To cancel an operation before starting its wait, start and settle `AuthRequest::Cancel { operation_id }`. General managed session concurrency remains unchanged; the status and input methods do not permit a second inference or mutation run.

Refresh and logout use `AuthRequest::Refresh { provider }` and `AuthRequest::Logout { provider }`, respectively, followed by the same `Session::wait` completion check. Always call `Session::shutdown` when the client finishes, including after errors.

Controlled endpoint tests establish protocol and cleanup behavior. They do not establish that a particular real account, client registration or subscription has been admitted; validate those separately for each provider. Model selection and credential precedence are described in [Models and API keys](models.md).
