# Models and credentials

The default `model-access` native package provides a model catalog, private credentials and inference. It supports OpenAI Responses, Chat Completions, Anthropic Messages and the [cloud and dedicated protocols](#cloud-and-dedicated-protocols) below. Subscription routes include Codex Responses (SSE/WebSocket), Copilot multi-protocol models and Radius pi-messages; see [account login](authentication.md). Unknown catalog protocols remain visible as unsupported; listing a configured model does not prove that the remote account can call it.

## CLI

Use the same installed composition for these commands. `--global-dir` selects the user configuration directory; `--session` selects durable history. API keys are read from stdin, never from a command-line value.

```sh
eden models list
eden auth set anthropic < /private/key-file
eden --session conversation.jsonl models select anthropic claude-sonnet-4-5 --thinking medium
eden --session conversation.jsonl 'Read the tests and fix the failure'
eden --session conversation.jsonl --model openai/gpt-4.1 'Continue the task'
eden --session conversation.jsonl models current
eden --session conversation.jsonl models cycle
eden models default openai gpt-4.1
eden models refresh
eden models source https://pi.dev
eden auth logout anthropic
```

Selection commits only while idle. Each run freezes provider, model, protocol, routing, limits and requested/effective thinking, including automatic compaction and branch summaries. A session's first resolved model is committed independently of the global default. Forks and branch navigation restore the selection on that history path. If a saved model disappears or lacks authentication, an available configured model can be selected with an explicit fallback event and committed record; without one, configure access before continuing.

The existing `OPENAI_MODEL`, `OPENAI_BASE_URL`, `EDEN_RESPONSES_PROFILE`, `EDEN_API_KEY_ENV`, `OPENAI_MAX_OUTPUT_TOKENS`, `OPENAI_REASONING_EFFORT` and `--env-file` path continues to use Responses. Selecting DeepSeek from the catalog uses its catalog protocol; it does not rewrite legacy history.

## Configuration and trust

Set `plugins.model-access` in the user `settings.json` or an explicitly trusted project's `.eden/settings.json`. The host excludes untrusted project settings before granting credential commands permission to execute. Native plugins are trusted code, not a sandbox.

```json
{
  "plugins": {
    "model-access": {
      "catalog": {
        "source": "https://pi.dev",
        "offline": false
      },
      "credentials": {
        "providers": {
          "openai": { "env": "OPENAI_API_KEY" },
          "anthropic": { "command": "my-secret-manager read anthropic" }
        }
      }
    }
  }
}
```

Catalog precedence is explicit configuration, selected remote catalog, then bundled Pi data. Source changes isolate caches. Refresh preserves valid cached data when a provider response fails, and never changes a target already frozen for a run. Offline catalog operations do not contact the network. The selected source supplies routing as well as metadata; remote data is never executed as a credential command. Explicit refresh is an asynchronous managed operation through the SDK and a waiting command through the CLI.

Credential precedence is an explicit private request value, the managed stored key or OAuth account, provider environment, then custom configuration. Logout removes managed storage; an environment key can remain configured. Keys and configured credential headers travel through the private credential service, outside model targets, request events and history. The store uses private filesystem permissions, a process lock and atomic replacement. Authentication operation IDs carry no key; key start/input and OAuth login/wait/submit/refresh/cancel/status are shared SDK operations. Stored OAuth expiry triggers a serialized refresh before inference or authenticated directory refresh; listing alone never refreshes. Refresh failure keeps the selected account and fails explicitly.

## Native authors and Rust consumers

`eden_protocol::models` defines `ModelSelection`, immutable `ModelTarget`, catalog requests/replies, private credential requests/replies, and authentication operations. `Session::models`, `model_selection`, `select_model`, `catalog` and `authenticate` expose these services; mutations return a run ID to await with `Session::wait`. Only a completed terminal confirms the operation settled. `Session::cancel` cancels a managed refresh or authentication operation. `Session::auth_status` and `submit_auth_input` expose the two side operations while a login wait is active.

The independently replaceable roles are `eden.model-catalog.v1`, `eden.credential-source.v1` and `eden.auth.v1`; inference remains `eden.coding-provider.v1`. A custom catalog changes actual provider routing and limits without replacing the provider. A custom credential source changes actual authorization without replacing the catalog. Credentials must never be emitted with `CallContext::emit` or copied into a target. The host redacts private call inputs from its automatic routing event. See [native plugins](native-plugins.md) for lifecycle and exact release pairing.

Completed provider reasoning records include their original provider/model/protocol identity. Matching targets can reuse opaque state; switching targets retains visible reasoning as assistant text and drops incompatible signatures. Legacy opaque state without enough identity fails with an explanatory error when safe projection is impossible. Tool IDs are mapped in matched call/result pairs. Unsupported images become a textual notice in the model projection while original attachments remain in history. Partial streamed tool arguments are never executable. Usage retains the raw provider payload alongside normalized counters, stop reason and sourced cost estimates; unknown counters remain distinct from zero.

## Catalog snapshot maintenance

The committed snapshot in `plugins/model-access/data` comes from the fixed `@earendil-works/pi-ai@0.85.1` release. Its provenance and upstream license are stored alongside it. Ordinary Cargo builds consume those resources without fetching a catalog or loading a JavaScript runtime. To update the snapshot, obtain an explicit fixed upstream package or retain the exact response bytes from the selected source, validate provider identities and supported routes, preserve provenance and notices, and submit the candidate data through a normal reviewed PR. Dynamic account-specific models are not fabricated as static entries.

`python scripts/verify-model-access.py` exercises installed consumers and independently compiled catalog/credential authors with controlled HTTP receivers. Real account access is verified separately from those fixtures.

## Cloud and dedicated protocols

Catalog-selected models also support Gemini (`google-generative-ai`), Vertex (`google-vertex`), Bedrock ConverseStream (`bedrock-converse-stream`), Azure Responses (`azure-openai-responses`) and native Mistral Chat (`mistral-conversations`). Gateways dispatch using each model's API identity, including OpenCode's Gemini models. All adapters return executable tool calls only after a successful complete response. Request cancellation closes the active transport; inference adds no SDK retries. Existing session retry policy owns retryable failures.

Use `GEMINI_API_KEY`, `GOOGLE_CLOUD_API_KEY`, `MISTRAL_API_KEY`, `AZURE_OPENAI_API_KEY` or `AWS_BEARER_TOKEN_BEDROCK`, or store a key with `eden auth set PROVIDER`. Vertex API keys use the express-mode publisher route; ADC uses the configured project and location. Azure sends its key in `api-key` and the deployment name as the request model. `AZURE_OPENAI_BASE_URL` or `AZURE_OPENAI_RESOURCE_NAME` supplies its endpoint, `AZURE_OPENAI_API_VERSION` optionally selects a version, and `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` maps comma-separated `model=deployment` pairs. The default is the v1 Responses API.

Non-secret provider routing can be configured explicitly. `catalog.providers.PROVIDER.compat` overlays protocol options, while `catalog.models` can define complete custom targets. Routing is frozen with the session's run target, including deployment, project, location and region.

```json
{
  "plugins": {
    "model-access": {
      "catalog": {
        "providers": {
          "azure-openai-responses": {
            "base_url": "https://RESOURCE.openai.azure.com/openai/v1",
            "compat": { "deployment": "DEPLOYMENT", "apiVersion": "v1" }
          },
          "google-vertex": {
            "compat": { "project": "PROJECT", "location": "global" }
          },
          "amazon-bedrock": {
            "compat": { "region": "us-east-1" }
          }
        }
      },
      "credentials": {
        "providers": {
          "amazon-bedrock": {
            "cloud": { "enabled": true, "profile": "development", "region": "us-east-1" }
          },
          "google-vertex": {
            "cloud": { "enabled": true, "quota_project": "PROJECT" }
          }
        }
      }
    }
  }
}
```

Bedrock supports bearer keys and the AWS credential chain, including environment keys, session tokens, shared profiles, assume-role and web identity. Region resolution uses explicit configuration, `AWS_REGION`/`AWS_DEFAULT_REGION`, then the selected local profile. Google ADC discovers `GOOGLE_APPLICATION_CREDENTIALS`, local application-default credentials or cloud metadata; `cloud.service_account_file` selects a service-account file explicitly; `cloud.adc_file` selects another supported ADC JSON file. Vertex ADC requires project and location, configurable as above or through `GOOGLE_CLOUD_PROJECT`/`GCLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`. Listing models does not fetch cloud tokens or contact metadata services; use `cloud.enabled: true` to declare metadata-based access in a cloud deployment. Authentication failures do not silently switch accounts.

Cloud SDK subprocess credential sources are not enabled. For a trusted credential command, use the existing `credentials.providers.PROVIDER.command` or private header configuration. Cloudflare AI Gateway uses `CLOUDFLARE_API_KEY` as `cf-aig-authorization`, with `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_GATEWAY_ID` filling its endpoint. Configure any additional upstream authorization in private credential headers, never in catalog headers.

`python scripts/verify-cloud-model-access.py` exercises installed CLI selection, tool and image results, signed Gemini state and reopened continuation through controlled Gemini, Vertex, Azure and Mistral endpoints. Bedrock tests exercise the actual AWS signing and transport stack. Remote model availability and cloud account permissions remain properties of the configured account.

## Subscription transports and account catalogs

Codex models have their own `openai-codex` identity and `openai-codex-responses` protocol. They send the account header through the private credential reply, retain complete conversation context, and use the dedicated `/codex/responses` route. `compat.transport` selects `auto` (default), `sse`, or `websocket`; auto falls back to SSE only before a server event has been received. Each request owns and closes its connection. No WebSocket session cache is retained across requests.

Copilot uses `github-copilot` with its selected model's Chat, Responses or Anthropic protocol. Login exchanges the GitHub user token for a short-lived inference token, checks model policy, and refresh repeats this exchange. Private account routing applies unless an explicit endpoint is configured. Eden identifies itself as Eden; any service-required integration headers must be configured for an admitted integration. A configured credential is not proof that an account or client has access.

`models refresh` fetches Radius `/v1/config` and Copilot `/models` when their credentials are configured. Radius returns the inference base URL and pi-messages models; it has no fabricated universal model list. Dynamic caches are separated by gateway and credential account scope; the previous valid account catalog survives a failed refresh and can be read offline. Refresh never changes the active run's frozen target. Configure the Radius gateway through `catalog.providers.radius.base_url` and `credentials.oauth.radius.gateway` together when using a different gateway. Explicit model/provider configuration remains highest priority.
