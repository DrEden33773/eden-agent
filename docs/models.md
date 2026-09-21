# Models and API keys

The default `model-access` native package provides a model catalog, private credentials and inference. It supports OpenAI Responses, Chat Completions and Anthropic Messages. Other catalog protocols remain visible as unsupported; listing a configured model does not prove that the remote account can call it.

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

Credential precedence is an explicit private request value, the managed stored key, provider environment, then custom configuration. Logout removes managed storage; an environment key can remain configured. Keys and configured credential headers travel through the private credential service, outside model targets, request events and history. The store uses private filesystem permissions, a process lock and atomic replacement. Authentication operation IDs carry no key; start/input/cancel/status are shared SDK operations. Subscription OAuth and cloud credential chains are separate adapters.

## Native authors and Rust consumers

`eden_protocol::models` defines `ModelSelection`, immutable `ModelTarget`, catalog requests/replies, private credential requests/replies, and authentication operations. `Session::models`, `model_selection`, `select_model`, `catalog` and `authenticate` expose these services; mutations return a run ID to await with `Session::wait`. Only a completed terminal confirms the operation settled. `Session::cancel` cancels a managed refresh or authentication operation.

The independently replaceable roles are `eden.model-catalog.v1`, `eden.credential-source.v1` and `eden.auth.v1`; inference remains `eden.coding-provider.v1`. A custom catalog changes actual provider routing and limits without replacing the provider. A custom credential source changes actual authorization without replacing the catalog. Credentials must never be emitted with `CallContext::emit` or copied into a target. The host redacts private call inputs from its automatic routing event. See [native plugins](native-plugins.md) for lifecycle and exact release pairing.

Completed provider reasoning records include their original provider/model/protocol identity. Matching targets can reuse opaque state; switching targets retains visible reasoning as assistant text and drops incompatible signatures. Legacy opaque state without enough identity fails with an explanatory error when safe projection is impossible. Tool IDs are mapped in matched call/result pairs. Unsupported images become a textual notice in the model projection while original attachments remain in history. Partial streamed tool arguments are never executable. Usage retains the raw provider payload alongside normalized counters, stop reason and sourced cost estimates; unknown counters remain distinct from zero.

## Catalog snapshot maintenance

The committed snapshot in `plugins/model-access/data` comes from the fixed `@earendil-works/pi-ai@0.85.1` release. Its provenance and upstream license are stored alongside it. Ordinary Cargo builds consume those resources without fetching a catalog or loading a JavaScript runtime. To update the snapshot, obtain an explicit fixed upstream package or retain the exact response bytes from the selected source, validate provider identities and supported routes, preserve provenance and notices, and submit the candidate data through a normal reviewed PR. Dynamic account-specific models are not fabricated as static entries.

`python scripts/verify-model-access.py` exercises installed consumers and independently compiled catalog/credential authors with controlled HTTP receivers. Real account access is verified separately from those fixtures.
