# Shared model input preparation

`eden-model-input` is the common backend for CLI, terminal, SDK and other callers. The host freezes a resolved model and configuration at the run boundary. Frontends display its effective budgets and image decisions rather than implementing their own preparation rules.

`BudgetSettings` stores global `reserve_tokens` and `keep_recent_tokens`, with optional independent overrides under `models[provider][model]`. `resolve(&target)` returns each effective value and its global or exact model source. Zero effective values are errors. Provider and model keys remain separate, so model identifiers containing slashes are unambiguous.

Capture each original `Block::Image` with `ImageRecord::new`, then call `prepare(choice, &target, &limits)`. `Auto` is the default for new images: it preserves aspect ratio to integer pixel precision, never upscales, and uses conservative fallback limits of 2048 pixels per side, 4,194,304 total pixels and 5 MiB encoded image bytes where the model has no explicit limit. Animated originals are preserved when compatible; adaptation that would discard animation frames fails explicitly. Adapted output uses lossless PNG; if necessary it progressively reduces dimensions to satisfy the image byte limit. PNG, JPEG, GIF and WebP inputs are recognized by their actual bytes, and mismatched media types or invalid data fail explicitly.

`Preserve` uses original bytes for a new image, subject to actual model limits. Historical `Auto` and `Preserve` retain the active sent version exactly, failing on incompatibility. `ReAdapt` explicitly creates a new version from the original. `Omit` explicitly removes the image from the current projection while retaining all original and previous payloads. A previously omitted historical record stays omitted until explicit re-adaptation. An unsupported model otherwise fails with guidance to omit or choose another model.

`ImageRecord` is serializable: persist the complete record with the committed context operation. Its payload table reuses equal original/sent content, and version entries carry the selected model, dimensions and a short explanation. `sent()` supplies the actual protocol image block for the request; retaining only that block loses the original and version history. Preparation returns a new record and leaves its input intact, so failure can preserve the draft and old branch.

Call `validate_images` over every projected image, including history and tool results, to enforce capabilities, per-image dimensions, pixel and byte limits, and total count. After the provider has serialized the complete request, pass its actual byte length as `body_bytes` to enforce `max_body_bytes`; a preliminary call with `None` cannot prove the wire-body constraint. Unknown constraints are `None`; they do not claim unlimited provider capacity. Request body overflow is an explicit failure requiring an adjusted context or model, never implicit image omission.

The backend performs no I/O, persistence or model discovery. The integration owns model constraint metadata, user choices, safe configuration boundaries, persistence, image previews, provider body measurement and error presentation. Run CPU-bound adaptation away from terminal rendering and async executor threads. The image decoder's default allocation limits apply; unsupported or oversized input fails before it can become a sent version.

```rust,ignore
let draft = ImageRecord::new(original_block)?;
let prepared = draft.prepare(ImageChoice::Auto, &target, &limits)?;
let projected: Vec<Block> = prepared.sent()?.cloned().into_iter().collect();
validate_images(&projected, &target, &limits, None)?;
// Commit `prepared` with the context mutation; send `projected`.
// Validate the actual serialized provider body before transport.
```
