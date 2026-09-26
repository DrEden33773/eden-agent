# Configuration description service

An instance may publish `eden.configuration.v1` alongside its existing services. The service uses `eden_protocol::configuration::PluginRequest` over the existing serialized service ABI. No manifest field or C ABI table changes are needed. The absence of this service keeps raw JSON configuration and ordinary instance restart available; it does not make the package uneditable.

`Describe` returns `Description`. `Validate { config }` returns `Validation` without changing effective state or initializing a replacement. `Update { config }` returns `Validation` after the live change has finished. An empty `errors` array means success. Field errors are `{ path, code, message }`, where `path` is an RFC 6901 JSON pointer. A service `Fault` remains distinct from field validation, including actual factory or initialization failure. Passing validation cannot guarantee successful initialization.

`Update` must be failure atomic: a field error, service fault or cancellation before successful completion leaves the previous effective configuration intact. The author must finish validation and fallible preparation before publishing changed state. Completion includes any required cleanup; an accepted asynchronous request is not an application receipt. Only fields covered by `live_paths` may use this operation. Any other changed field requires the host's local replacement transaction. Arrays are replaced as a whole; a live declaration for an array therefore names the array, rather than selected indexes.

## Description and schema

`Description` contains `schema`, `defaults`, `description`, `secret_paths`, `live_paths` and `editable_layers`. Defaults are informational: validation does not insert them into the candidate or change the existing recursive object merge and array replacement rules. Layer names are `global`, `trusted_project` and `explicit`; an empty editable-layer list defers to existing host policy. Provenance, the concrete edit target, effective values and revision belong to host inspection state, not plugin lifecycle scope. Describing a layer does not establish project trust or authorize native installation or activation.

An absent schema accepts any JSON value. An explicit schema is a boolean or an object using this supported subset:

- `type`: one of `object`, `array`, `string`, `boolean`, `number`, `integer` or `null`.
- `properties`, `required`, boolean `additionalProperties`, and one schema in `items`.
- A nonempty `enum` array, numeric `minimum` and `maximum`.
- Nonnegative integer `minLength`, `maxLength`, `minItems` and `maxItems`. String length counts Unicode scalar values.
- Informational `title`, `description` and `default`.

Keywords follow JSON Schema's type-specific behavior: for example, `properties` constrains object values; use `type: "object"` to require an object. Unsupported keywords, malformed keyword values and malformed pointers cause `configuration_schema` faults, even in unused optional properties. Constraints such as references, unions, patterns and conditional schemas must not be advertised as host-validated. Authors can perform additional business validation in the service and return field errors. Complex configurations remain editable as raw JSON without a schema; the later form layer may provide a fallback rather than pretending every structure is a simple form.

## Secret boundaries

`secret_paths` and `live_paths` are RFC 6901 pointers; the empty pointer denotes the entire value. A live path covers its whole subtree. Schema annotations, descriptions, enum options and field-error messages must never contain secret values. Default configuration must be redacted before public inspection, just like effective configuration. Author validation errors must describe the rule without echoing rejected input.

The `redact` helper clones a private value and replaces declared secret subtrees with JSON null, preserving object and array shape. A malformed pointer redacts the entire document. Null in a public snapshot is a redaction marker, not a public instruction to clear a secret. The host preserves existing secrets privately when constructing an ordinary edit candidate and rejects public patches that explicitly contain a secret path, even if its value is unchanged. Configuration service payloads are redacted from public routing traces and request Debug output. `validate_public_edit` rejects changes and removals of declared secrets by comparing complete private old and new candidates. Secret changes require a separate private-input operation; ordinary editing cannot replace, clear or expose them. Keep private values out of history, public traces, application receipts and retry records.

The `validate` helper checks the descriptor and ordinary value constraints without side effects. `is_live_change` classifies a validated full candidate against its old value. These helpers supply shared behavior for headless management and later forms; they do not implement replacement, persistence, recovery or trust decisions themselves.

## Headless CLI and RPC

The current edit target is an explicit session override. Applying a patch does not rewrite global settings, trusted-project settings or the source composition file. Successful applies persist the override, composition binding and completed receipt together when the session has a history store; reopening replays that committed override. `config status` in a later CLI process can therefore recover saved successful receipts. Failed application receipts are retained by the running Session; durable failure reporting also requires a usable history store. Preserve the returned failure receipt when storage itself has failed.

Replacement selects an independently installed native package version through the supplied resolved composition. Keep old libraries and package directories installed while existing sessions or automatic recovery may reference them. A local replacement stops and rebuilds the affected instances; it does not unload native libraries from the process or provide process-restart isolation.

The CLI and version 1 JSONL RPC use the same `Session` configuration methods as SDK callers. They do not start a conversation run. With an installed composition and an existing saved session, first inspect the stable instance id and current revision:

```sh
eden --composition /path/to/composition.json --session /path/to/session.jsonl --json config inspect
eden --composition /path/to/composition.json --session /path/to/session.jsonl --json config validate INSTANCE --revision 0 --patch '{"enabled":true}'
eden --composition /path/to/composition.json --session /path/to/session.jsonl --json config preview INSTANCE --revision 0 --patch '{"enabled":true}'
eden --composition /path/to/composition.json --session /path/to/session.jsonl --json config apply INSTANCE --revision 0 --patch '{"enabled":true}' --mode wait
eden --composition /path/to/composition.json --session /path/to/session.jsonl --json config status 1
```

Replace `INSTANCE`, revision `0` and operation `1` with inspected or returned identities, and choose fields the selected plugin accepts. `--patch` uses the existing recursive object merge; arrays replace their previous value. Omitted secret fields are retained privately. `--replacement /path/to/resolved-composition.json` selects an already installed replacement; it does not install or build code. `--mode wait` preserves affected foreground work through its safe boundary. Explicit `--mode cancel` cancels that work before applying.

CLI `apply` waits for the final receipt and exits successfully only for `applied`; recovery and failure receipts remain available on stdout with a nonzero exit code. `validate` returns field errors and a nonzero exit code for invalid values. CLI `apply` and `status` require `--session` so changes and retained receipts belong to a saved session. Inspection, validation and preview can run without a saved session. For ongoing management of an in-memory Session, use RPC.

Start `eden --composition /path/to/composition.json --session /path/to/session.jsonl rpc`. The initial `ready` frame supplies `session_id`. Send one JSON object per line, substituting that identity:

```json
{"version":1,"id":"inspect","session_id":42,"method":"config.inspect","params":{}}
{"version":1,"id":"validate","session_id":42,"method":"config.validate","params":{"instance":"INSTANCE","revision":0,"patch":{"enabled":true}}}
{"version":1,"id":"preview","session_id":42,"method":"config.preview","params":{"instance":"INSTANCE","revision":0,"patch":{"enabled":true}}}
{"version":1,"id":"apply","session_id":42,"method":"config.apply","params":{"instance":"INSTANCE","revision":0,"patch":{"enabled":true},"mode":"wait"}}
{"version":1,"id":"status","session_id":42,"method":"config.status","params":{"operation":1}}
```

Wait for inspection before constructing a revision-bound edit. Validate and preview replies are ordinary `result` frames. A successful `config.apply` request returns an `accepted` frame with top-level `operation`, without `run_id`; it acknowledges management ownership, not completed application. Query `config.status` for the receipt until its status settles. A stale revision returns a structured conflict error. The optional `replacement` field is the resolved composition path; omitted `mode` means `wait`. Configuration values and private secret input do not belong in operation receipts.

## SDK ownership and completion

`Session::inspect_configuration`, `validate_configuration(Change)`, `preview_configuration(Change)` and `apply_configuration(Change, ApplyMode)` share the CLI/RPC model. `apply_configuration` returns a management operation id; use `configuration_operation` or `wait_configuration` for its receipt. The Session retains accepted execution when a caller drops its waiter. Session shutdown cancels active foreground work, then waits for accepted configuration operations before destroying instances.

Preview reports the dependency/ownership closure, affected foreground run ids and managed jobs. Inspect reports installed but disabled packages, missing dependency contracts, whether running instances are selected, generations, per-field provenance, redacted effective values and operation results. The explicit override is separate from global and trusted-project files; the existing recursive object merge and array replacement behavior is unchanged.

Waiting keeps old effective bindings usable for existing runs and rejects new affected submissions. At the safe boundary, managed background jobs are cancelled and joined; dependents stop before their providers. Public calls to affected instances remain gated until the new routing and durable binding agree. Unrelated calls, jobs and frontend attachments retain their ownership. A changed instance's old presentation actions cannot execute against its replacement generation.

`applied` means application and binding commit completed. A reversible application failure tries the previous configuration once: `restored` reports successful recovery, `recovery_failed` reports both faults, and `cleanup_failed` means the cleanup barrier prevented any replacement. Recovery does not replay prior tool effects or resurrect old task/connection state. A no-op is `unchanged` in the preview and does not recreate its instance. Configuration revision advances when an accepted transaction settles, including recovery, so clients refresh before retrying. Old native versions remain retained by package references.

Live updates require the optional configuration service and declared live fields. Native recreation supports configuration and compatible code-version changes with unchanged routing/service declarations. Topology changes use explicit composition switching. Irreversible state migrations retain their separate explicit migration path. Form construction and private secret editing build on this backend in their own frontend delivery.
