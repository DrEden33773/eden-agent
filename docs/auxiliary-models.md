# Auxiliary inference and cache warming

Auxiliary inference uses `eden.auxiliary-model.v1`, independently of the foreground provider wrapper chain, chat attempts, queue consumption and tool execution. The default installation includes `cache-warmer` with `mode: "off"`. An independent build of the actual plugin source under `tests/contract-authors/cache-warmer` consumes only the public SDK.

## Prepare and replay

`Prepare` accepts a logical `coding::ModelInput` in a foreground invocation. The provider transforms its prefix and tools once and retains the resulting payload privately. `Latest` and `Prepare` return an optional `auxiliary::Snapshot`; `Replay` returns `coding::ModelReply`. The default provider also captures ordinary foreground requests after their provider wrappers have transformed the input. Summary requests with an explicit output limit do not publish warming snapshots. Unsupported targets return no snapshot and ordinary foreground calls publish `auxiliary_unavailable`.

The first supported routes are catalog-selected `openai` and `deepseek` with `api: "openai-completions"`. Legacy Responses configuration, Codex, Anthropic, cloud routes and arbitrary OpenAI-compatible providers are unsupported for this replay contract. `Generate` currently supports the same routes and requires a nonempty auxiliary purpose, an explicit positive output budget, and a bounded timeout.

A handle binds the provider incarnation, host context revision, original foreground run and provider token. The provider retains at most one prepared request; handles are ephemeral and are never history recovery data. Credentials are not captured: each send obtains current credentials through `eden.credential-source.v1`. Replay bypasses foreground wrappers and prefix projection, preserves the prepared body and tool schemas, and changes only the output-limit field. Endpoint authentication is refreshed separately. Output is returned only to the auxiliary caller; the warmer discards all items, including tool calls.

`runtime::SnapshotRevision` reads the current policy-neutral context identity; `InvocationSnapshotRevision` reads the identity fixed at invocation admission. Preparing an old request cannot grant it a newer identity. SDK `invalidate_snapshot()` cancels and joins auxiliary calls after a strategy commits a new projection; it does not depend on the default summary's record sequence. New input, steering/follow-up, selected model changes, navigation, resource reload, projection changes and replacement of selected context/provider/resource instances retire old snapshots. Disabling the warmer retires its owner and jobs without invalidating an unrelated caller's provider-owned handle. Composition replacement destroys the old provider state.

The host checks replay incarnation/revision and positive budgets before admission. Timeouts are limited to 120 seconds and output requests to 4096 tokens; the provider also applies model limits. On timeout or invalidation the host cancels and waits for native cleanup. Auxiliary implementations cannot call the foreground provider chain or invalidate themselves. Streaming replay additionally binds admission to the original active foreground run; settlement closes that admission and waits for cleanup. Idle replay has a session/instance lifetime and can outlive a turn.

## Configuration and status

Use the existing SDK, CLI or RPC configuration management authority to inspect, validate, preview and apply `cache-warmer` configuration. Every field requires local restart; invalid values leave the old instance active. Unrelated provider and store instances keep their generations. No configuration file watcher or parallel apply path is introduced.

| Field | Default | Meaning |
| --- | --- | --- |
| `mode` | `off` | `off`, `streaming` while the originating turn runs, or `idle` across turns |
| `interval_ms` | 60000 | Delay between warming attempts; missed ticks are never caught up |
| `ttl_ms` | 300000 | Fixed lifetime measured from preparation, at most one hour |
| `safety_ms` | 15000 | Stop starting requests before the TTL boundary |
| `timeout_ms` | 10000 | Per-request limit, no greater than the safety interval |
| `max_requests` | 3 | Maximum warming attempts for one snapshot, at most 100 |
| `max_output_tokens` | 1 | Output allowance for one warming request, at most 128 |

TTL is an explicit user policy, not a promise about a provider's actual cache retention. The timer never extends the preparation deadline after a request succeeds or after a plugin restart. `eden.cache-warmer.v1` accepts `{"op":"status"}` and `{"op":"cancel"}` through ordinary SDK service handles. Status includes the last auxiliary usage/error, request count and job identity. Explicit cancel joins jobs and stops automatic warming until configuration restarts the instance. Cleanup failures remain observable and prevent replacement work. Long sessions bootstrap from the current event cursor; lag causes resynchronization rather than abandonment.

`auxiliary_started`, `auxiliary_usage` and `auxiliary_finished` include purpose, call identity, owner and snapshot metadata, independently of foreground `model_*` events. Normal output-budget exhaustion reads the usage tail and discards incomplete items. These observations are available through the ordinary SDK/RPC event stream; they do not write foreground model responses, interrupted attempts or compaction calibration.

## Verification and limits

`python scripts/verify-cache-warmer.py` freezes the host before building the independent plugin, then observes actual HTTP bodies and peer EOF. It exercises idle across turns, streaming settlement, foreground priority, stale snapshots, output/tool isolation, separate usage and A2 local configuration changes. Provider tests cover projection reuse and credential refresh. Kernel and virtual-clock tests cover admission/cleanup races, fixed deadlines and missed windows. The suite is part of three-platform installed acceptance.

Controlled usage fields prove transport and accounting mechanisms only. No real-provider cache hit, reduced latency, fee saving or net benefit is claimed from these fixtures or from HTTP success. A live benefit measurement must compare the same provider/model/prefix with warming disabled and enabled, report provider cache-read tokens and every extra request's cost, and separately report latency. No live-account measurement is included in this delivery.
