# Local models with llama.cpp

Eden's `model-access` package exposes llama.cpp through the independently replaceable `eden.model-manager.v1` role. The default catalog consumes that role's observations; the default Chat Completions provider performs inference. No key is required for an unauthenticated local server.

## Configure and run

Use a llama.cpp server with router management endpoints. The integration is verified against commit `58367713a6935c0810103378144008df32e3d5db`. Start a router without `-m`, with explicit model storage and unlimited loaded-model capacity to preserve other clients' models:

```sh
llama-server --host 127.0.0.1 --port 8080 --models-dir /path/to/gguf-models --models-max 0 --no-models-autoload -c 4096
```

Set `LLAMA_BASE_URL=http://127.0.0.1:8080`, or put this in your user `settings.json`:

```json
{
  "plugins": {
    "model-access": {
      "router": { "url": "http://127.0.0.1:8080" }
    }
  }
}
```

`LLAMA_API_KEY`, `eden auth set llama.cpp`, or `credentials.providers.llama.cpp` supplies optional private authorization for both management and inference. URL credentials are rejected. `router.offline` or `catalog.offline` prevents router networking and projects no remote models. `router.search_url` can select another Hugging Face-compatible search endpoint; router credentials are never forwarded to it.

```sh
eden router list
eden router search SmolLM2-135M-Instruct
eden router search bartowski/SmolLM2-135M-Instruct-GGUF
eden --json router download bartowski/SmolLM2-135M-Instruct-GGUF:Q4_K_M
eden --json router load bartowski/SmolLM2-135M-Instruct-GGUF:Q4_K_M
eden models list
eden --session local.jsonl models select llama.cpp bartowski/SmolLM2-135M-Instruct-GGUF:Q4_K_M
eden --session local.jsonl 'Describe this project'
eden router unload bartowski/SmolLM2-135M-Instruct-GGUF:Q4_K_M
eden router reconnect
```

Search returns repository identities, download counts and gated status. Searching an exact `owner/repo` also returns available quantizations, GGUF filenames and total shard sizes (unknown if any size is missing); projectors are excluded and Q4_K_M is listed first. Pass an exact `owner/repo:quant` to download when you know the repository. The server downloads files using its own Hugging Face credentials (`HF_TOKEN`); a client key does not grant gated download access. Unload and cancel stop a model without deleting its files.

Loaded and sleeping models are selectable. Unloaded presets are selectable only when router autoload is enabled and the model has not failed. Ordinary unloaded models and loading/downloading models remain visible in `router list` but are excluded from model selection. The target freezes reported context limits and image support, with the fixed Pi fallback when metadata is absent; a running request keeps its target even if the catalog changes.

## Completion, cancellation and reconnect

`--json` prints ordered `model_management` events followed by the final result. `accepted` means the server accepted the request; `completed` means Eden observed the requested remote state. Numeric download progress comes from the server's SSE stream. HTTP errors and server error bodies are not echoed as credentials may appear in them.

Ctrl-C cancels the managed operation. Eden retains an in-flight POST until it settles, then sends unload and confirms stopped state before cleanup finishes. `eden router cancel MODEL` explicitly stops a named remote operation, including one started by an earlier process. If the connection cannot confirm stopping, the terminal reports `RemoteStateUnknown` in its failure or cleanup errors. Reconnect performs fresh reads only; it never repeats load or download. Graceful SDK shutdown follows the same cleanup path. Forced process termination cannot perform network cleanup; reconnect and inspect the server.

By default load preserves other models. A finite-capacity router can evict models inside its own load handler, so this mode requires `--models-max 0`. Explicit `eden router load MODEL --unload-others` first unloads the other observed loaded/sleeping models. Operations address named models; cancellation can stop another client's activity on that same named model, so coordinate ownership on shared servers.

## Rust and native consumers

`Session::managed_models().await` reads a `ManagerReply`. `Session::manage_models(ManagerRequest::Load { model, unload_others: false })` returns a run ID. Await `Session::wait(run)` and inspect both outcome and `cleanup_errors`; use `Session::cancel(run)` to request cancellation. `Session::events_after(sequence)` exposes ordered `model_management` events. `ManagerRequest::Reconnect` refreshes observations, and selection remains an explicit `Session::select_model` operation.

An independent SDK author registers `MODEL_MANAGER` with `Package::service`, returns safe `ManagedModel` targets and manages cancellation through `CallContext::scope`. A selectable target contributes to the default catalog, including actual routing and limits. The selected credential source and provider remain separate roles. See [the author example](../tests/contract-authors/model-services/src/lib.rs) and [model contracts](models.md#native-authors-and-rust-consumers).

`python scripts/verify-router-models.py` runs controlled installed CLI/SDK lifecycle and independent-manager checks. A real server is a separate verification environment; hosted fixtures do not download models or claim hardware/model quality coverage.
