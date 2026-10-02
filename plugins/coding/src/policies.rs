//! Ordered request transforms fail closed and use the same transactions as manual edits.
use super::*;
use eden_plugin_sdk::protocol::context_edit as e;

pub(crate) async fn run(
    input: &ContextInput,
    boundary: e::Boundary,
    cx: &CallContext,
    settings: &Settings,
) -> Result<(), Fault> {
    for policy in settings
        .policies
        .iter()
        .filter(|policy| policy.enabled && policy.boundary == boundary)
    {
        let snapshot = edits::service(
            e::Request::Inspect {
                input: input.clone(),
            },
            cx.clone(),
            settings.clone(),
        )
        .await?;
        let mut protected: Vec<String> = snapshot
            .effective
            .entries
            .iter()
            .filter(|entry| {
                snapshot
                    .original
                    .entries
                    .iter()
                    .find(|original| original.id == entry.id)
                    .is_none_or(|original| original.item != entry.item)
            })
            .map(|entry| entry.id.clone())
            .collect();
        let history: StoreReply = store_call(&cx, StoreRequest::Read).await?;
        if let Some(checkpoint) = eden_plugin_sdk::protocol::history::active_path(&history.records)?
            .iter()
            .rev()
            .find(|record| record.kind == "compaction")
        {
            protected.extend(
                checkpoint.payload["context_protected"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        let policy_input = e::PolicyInput {
            config: policy.config.clone(),
            boundary,
            document: snapshot.effective.clone(),
            revision: snapshot.revision.clone(),
            protected,
        };
        let outcome: Result<e::PolicyOutput, Fault> = if policy.role == "large_tool_output" {
            trim_large(policy_input, &policy.config)
        } else {
            cx.call(&policy.role, &policy_input).await
        };
        let output = outcome
            .map_err(|error| Fault::new("ContextPolicyFailure", &policy.name, error.to_string()))?;
        output
            .document
            .validate()
            .map_err(|error| Fault::new("ContextPolicyFailure", &policy.name, error.to_string()))?;
        if serde_json::to_value(&output.document).ok()
            == serde_json::to_value(&snapshot.effective).ok()
        {
            continue;
        }
        if boundary != e::Boundary::BeforeRequest && output.scope == e::Scope::NextRequest {
            return Err(Fault::new(
                "ContextPolicyFailure",
                &policy.name,
                "temporary transforms require the before_request boundary",
            ));
        }
        edits::service(
            e::Request::Apply {
                input: input.clone(),
                edit: e::Apply {
                    revision: snapshot.revision,
                    document: output.document,
                    scope: output.scope,
                    source: format!("policy:{}", policy.name),
                },
            },
            cx.clone(),
            settings.clone(),
        )
        .await
        .map_err(|error| Fault::new("ContextPolicyFailure", &policy.name, error.to_string()))?;
        cx.emit(
            "context_policy",
            json!({ "policy": policy.name, "boundary": boundary }),
        )?;
    }
    Ok(())
}
fn trim_large(input: e::PolicyInput, config: &Value) -> Result<e::PolicyOutput, Fault> {
    let threshold = config["threshold_chars"].as_u64().unwrap_or(16000) as usize;
    let keep = config["keep_chars"].as_u64().unwrap_or(4000) as usize;
    let recent = config["keep_recent_results"].as_u64().unwrap_or(2) as usize;
    if keep == 0 || threshold <= keep {
        return Err(Fault::new(
            "InvalidInput",
            "large_tool_output",
            "threshold_chars must exceed positive keep_chars",
        ));
    }
    let mut document = input.document;
    let count = document
        .entries
        .iter()
        .filter(|entry| matches!(entry.item, Item::ToolResult { .. }))
        .count();
    let mut seen = 0;
    for entry in &mut document.entries {
        if let Item::ToolResult { result, .. } = &mut entry.item {
            seen += 1;
            if seen > count.saturating_sub(recent)
                || input.protected.contains(&entry.id)
                || result.text.chars().count() <= threshold
            {
                continue;
            }
            let prefix: String = result.text.chars().take(keep / 2).collect();
            let suffix: String = result
                .text
                .chars()
                .rev()
                .take(keep - keep / 2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            result.text = format!(
                "{prefix}\n[Earlier output shortened; original history entry {}]\n{suffix}",
                entry.id
            );
            result.truncated = true;
        }
    }
    Ok(e::PolicyOutput {
        document,
        scope: e::Scope::Branch,
    })
}
