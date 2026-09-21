use super::*;
use serde_json::json;

#[test]
fn only_routable_models_are_selectable() {
    let data = json!({
        "data": [
            { "id": "loaded", "status": { "value": "loaded" } },
            { "id": "sleep", "status": { "value": "sleeping" } },
            { "id": "preset", "source": "preset", "status": { "value": "unloaded" } },
            { "id": "disk", "source": "cache", "status": { "value": "unloaded" } },
            {
                "id": "broken",
                "source": "preset",
                "status": { "value": "unloaded", "failed": true },
            },
            { "id": "busy", "status": { "value": "loading" } },
            { "id": "download", "status": { "value": "downloading" } }
        ],
    });
    let disabled = snapshot(
        &data,
        &json!({ "models_autoload": false }),
        "http://localhost:8080",
    )
    .unwrap();
    assert_eq!(
        disabled
            .models
            .iter()
            .filter(|m| m.selectable)
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["loaded", "sleep"]
    );
    let enabled = snapshot(
        &data,
        &json!({ "models_autoload": true }),
        "http://localhost:8080",
    )
    .unwrap();
    assert_eq!(
        enabled
            .models
            .iter()
            .filter(|m| m.selectable)
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["loaded", "sleep", "preset"]
    );
    assert_eq!(enabled.models.len(), 7);
}

#[test]
fn malformed_snapshot_cannot_confirm_remote_stop() {
    assert!(snapshot(&json!({ "error": "bad" }), &json!({}), "http://localhost").is_err());
    assert!(
        snapshot(
            &json!({ "data": [{ "id": "x" }] }),
            &json!({}),
            "http://localhost"
        )
        .is_err()
    );
}

#[test]
fn progress_combines_shards_without_exposing_download_urls() {
    assert_eq!(
        progress(&json!({
            "https://private/one": { "done": 20, "total": 40 },
            "two": { "done": 30, "total": 60 },
        })),
        Some(0.5)
    );
    assert_eq!(progress(&json!({ "one": { "done": 0, "total": 0 } })), None);
}

#[test]
fn projection_uses_reported_limits_and_image_capability() {
    let reply = snapshot(
        &json!({
            "data": [{
                "id": "vision",
                "status": { "value": "loaded" },
                "meta": { "n_ctx": 2048, "n_ctx_train": 4096 },
                "architecture": { "input_modalities": ["text", "image"] },
            }],
        }),
        &json!({ "max_instances": 0 }),
        "http://localhost",
    )
    .unwrap();
    assert_eq!(reply.models[0].target.limits.context_window, 2048);
    assert!(reply.models[0].target.capabilities.images);
    assert_eq!(reply.max_instances, Some(0));
}

#[test]
fn actual_load_progress_retains_value() {
    assert_eq!(
        progress(&json!({ "stages": ["model", "context"], "current": "model", "value": 0.4 })),
        Some(0.4)
    );
}

#[test]
fn repository_groups_shards_and_keeps_unknown_sizes_unknown() {
    let repo = repository(&json!({
        "id": "owner/repo",
        "siblings": [
            { "rfilename": "model-Q4_K_M-00001-of-00002.gguf", "size": 100 },
            { "rfilename": "model-Q4_K_M-00002-of-00002.gguf", "size": 200 },
            { "rfilename": "mmproj-Q8_0.gguf", "size": 900 },
            { "rfilename": "model-UD-IQ2_XXS.gguf" }
        ],
    }))
    .unwrap();
    assert_eq!(repo.quants.len(), 2);
    assert_eq!(repo.quants[0].name, "Q4_K_M");
    assert_eq!(repo.quants[0].bytes, Some(300));
    assert_eq!(repo.quants[1].bytes, None);
}
