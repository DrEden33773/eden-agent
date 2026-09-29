# Shared context and model input

`Session::inspect_context()` returns a read-only versioned view of original and effective input, the last actual model request, configured policies, and effective model budget. The terminal's `/context` command and F7 use the same API through the typed local host client. Inspection never starts a model request or consumes a temporary edit.

## Editing and scope

An editable `Document` contains ordered, stable `Entry` identities and advertised tool definitions. System and developer messages are entries too. `Session::edit_context(Apply)` submits the complete reviewed candidate with its `Revision`, source label, and `Scope::Branch` or `Scope::NextRequest`. New entries use unique `inserted:` identities. Tool calls and results must remain complete ordered groups. Invalid candidates and stale revisions commit nothing; refresh and explicitly rebase the retained draft before applying again.

Branch edits are append-only transactions. Original messages, tool arguments/results and external effects remain in history. An edit changes model input and never executes a historical tool. The executor's tool permissions and scheduling remain authoritative even when advertised definitions are edited. Unchanged entries continue to use their current facts, including a tool result that arrives after a preview.

Edits accepted during a run take effect at a subsequent model-request boundary. The already sent request and started tools are unchanged. A next-request edit is claimed atomically with one logical request and remains frozen through its retries. Its claim persists even if the request fails. Overflow recovery for an edited request summarizes only that request's effective input without making temporary edits into permanent branch summaries.

## Policies and compaction

The coding package accepts an ordered `context_policies` array. Each entry contains `name`, native service `role`, `boundary` (`before_request`, `tool_round_end`, or `run_finish`), `enabled`, and `config`. Services receive `context_edit::PolicyInput` and return `PolicyOutput`; later policies consume prior valid changes. Persistent results use the same checked edit transaction as manual changes. Temporary policies run at `before_request`. `run_finish` runs once at successful foreground completion after the input queues drain; cancellation does not start new policy work. Invalid output or service failure stops the affected operation and identifies the policy; unedited input is never used as a fallback.

The bundled `large_tool_output` policy is disabled by default. Its defaults are `threshold_chars: 16000`, `keep_chars: 4000`, and `keep_recent_results: 2`. It deterministically retains the beginning and end of older large tool text and links the original history entry. Explicit edits are protected, including retained edited entries after compaction. The complete original result and its artifacts remain available.

Manual compaction uses `Session::compact` and the terminal's `/compact`. Existing summary and optional notes policies remain available. Edited compaction summarizes effective content and records retained effective entries and absorbed transaction identities, so an old edit cannot resurrect compacted originals. `compaction::Request.effective_prefix`, when supplied, is the coordinator's authoritative material for a policy's summary. Failure leaves the old checkpoint intact.

`Session::rebuild_context` reconstructs original records and explicitly selected persistent edits on a fresh branch in one checked transaction. `/context-rebuild` exposes the branch and edit selection. Rebuilding itself does not call a model or replay tools. The source branch stays available for comparison; an explicit subsequent compaction generates a new summary when desired.

## Budgets and images

Global `compaction.reserve_tokens` and `keep_recent_tokens` retain their defaults of 16384 and 20000. `compaction.models[provider][model]` can override either independently. The context view reports each effective value and its source. Model selection is frozen for a run; configuration changes follow the existing shared safe-boundary rules.

`images` accepts `mode` (`auto` or `preserve`), `limits`, and nested `models[provider][model]` overrides. Limits include maximum dimensions, pixels, per-image bytes, image count, and serialized request-body bytes. Catalog `compat.image_limits` are hard upper bounds. New images default to proportional adaptation without upscaling. History retains original bytes and each sent version; changing models validates the existing sent version without silently resizing it. Unsupported images fail with an explicit preserve, omit, re-adapt or change-model path. Animated images that require resizing are refused rather than silently flattened.

`Session::edit_images` applies an explicit `ImageEdit` against the reviewed revision. Omission is persistent and reversible; re-adaptation creates another retained version. The terminal previews original and sent images and exposes these actions. Shared codec and budget APIs are documented in [model input](model-input.md). HTTP transports check the final serialized JSON body against configured byte limits. Bedrock and the targetless legacy route refuse an explicit serialized-body byte cap because they do not expose the required per-request limit enforcement path. The legacy Responses route retains its existing image-block support; known catalog model capabilities remain authoritative.

## Fixed session references

`Session::session_catalog`, `session_branches`, and `reference_preview` read saved sources without opening a writer or changing their active branch. `Preview::freeze` captures selected effective entries and explicitly chosen images. Source tool exchanges become quoted material; tool definitions and provider thinking are excluded. The snapshot remains usable if its source changes or disappears.

Submit frozen `Reference` values through `submit_referenced` or `enqueue_referenced`; ordinary `submit_blocks` and `enqueue` remain convenience forms without references. References persist with user messages and queue entries. The actual request compares its final system with the captured source system: equal prompts are omitted, different prompts are included as labelled source material, and missing historical snapshots remain explicitly unknown. References never replace the target system prompt.

Reference budget checks refuse excessive selections without truncating or automatically summarizing them. The local host checks references and image compatibility before accepting a prompt so the terminal retains a rejected draft; the request boundary checks again against actual effective input. SDK callers can use `check_reference_input` for the same preview. Reduce selected entries/images or explicitly change the context/model before resubmitting.

## Native compatibility

This release uses host/SDK pairing `eden-native-0.13.0`; rebuild native libraries and manifests together. The byte ABI table remains version 1. New wire fields have defaults for old history. `StoreRequest::AppendChecked.new_branch` lets stores atomically fork at the checked head before appending, refusing an existing destination branch. Saved-context references retain their source identities; copies remap local edit and image entry identities with the rest of the tree.
