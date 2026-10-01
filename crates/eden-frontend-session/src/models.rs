//! Eden accepts requested effort levels and remains the only compatibility mapping authority.
use crate::{Adapter, fault, projection::text};
use eden_protocol::Fault;
use serde_json::{Value, json};

fn grok_effort(effort: &Value) -> Value {
    if effort == "off" {
        json!("none")
    } else {
        effort.clone()
    }
}

fn model_info(entry: &Value, current: &Value) -> Value {
    let model = &entry["target"];
    let thinking = if model["provider"] == current["provider"] && model["model"] == current["model"]
    {
        &current["thinking"]
    } else {
        &model["thinking"]
    };
    let efforts: Vec<_> = ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .map(
            |effort| json!({ "id": effort, "value": grok_effort(&json!(effort)), "label": effort }),
        )
        .collect();
    json!({
        "modelId": format!("{}/{}", text(&model["provider"]), text(&model["model"])),
        "name": format!("{}/{}", text(&model["provider"]), text(&model["model"])),
        "description":
            format!("{} · {}", text(&entry["name"]), text(&entry["status"])),
        "_meta": {
            "totalContextTokens": model["limits"]["context_window"],
            "acceptsImages": model["capabilities"]["images"],
            "supportsReasoningEffort": true,
            "reasoningEfforts": efforts,
            "reasoningEffort": grok_effort(&thinking["requested"]),
            "edenThinking": thinking,
        },
    })
}

impl Adapter {
    pub(crate) async fn model_state(&self) -> Result<Value, Fault> {
        let snapshot = self.view.lock().await.snapshot.clone();
        if snapshot.state.read_only {
            let current = snapshot
                .history
                .iter()
                .rev()
                .find(|r| r.kind == "model_selection")
                .map(|r| r.payload["target"].clone())
                .unwrap_or(Value::Null);
            return Ok(json!({
                "currentModelId":
                    format!("{}/{}", text(&current["provider"]), text(&current["model"])),
                "availableModels":
                    if current.is_null() {
                        vec![]
                    } else {
                        vec![model_info(
                            &json!({
                                "target": current,
                                "name": "Saved model",
                                "status": "read-only",
                            }),
                            &current,
                        )]
                    },
            }));
        }
        let catalog = self.post("/models/list", json!({})).await?;
        let current = self.current_model_state().await?;
        let target = &current["effective_target"];
        let models: Vec<_> = catalog["models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| model_info(entry, target))
            .collect();
        Ok(json!({
            "currentModelId": if target.is_null() {
                    "eden/unavailable".into()
                } else {
                    format!("{}/{}", text(&target["provider"]), text(&target["model"]))
                },
            "availableModels": models,
            "_meta": { "edenDiagnostic": current["diagnostic"] },
        }))
    }

    async fn current_model_state(&self) -> Result<Value, Fault> {
        match self.post("/models/current", json!({})).await {
            Ok(state) => Ok(state),
            Err(error) if error.code == "ModelUnavailable" => Ok(json!({
                "effective_target": null,
                "diagnostic": error.to_string(),
            })),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn model_selection(&self, params: &Value) -> Result<Value, Fault> {
        let (provider, model) = text(&params["modelId"])
            .split_once('/')
            .ok_or_else(|| fault("Invalid Eden model identity"))?;
        let mut selection = json!({ "provider": provider, "model": model });
        let current = self.current_model_state().await?;
        let effort = params["_meta"]["reasoningEffort"]
            .as_str()
            .or_else(|| current["effective_target"]["thinking"]["requested"].as_str());
        if let Some(effort) = effort {
            selection["thinking"] = json!(if effort == "none" { "off" } else { effort });
        }
        Ok(selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_requests_are_offered_while_effective_thinking_is_preserved() {
        let current = json!({
            "provider": "test",
            "model": "sparse",
            "thinking": { "requested": "minimal", "effective": "high" },
        });
        let model = model_info(&json!({ "target": current }), &current);
        assert_eq!(
            model["_meta"]["reasoningEfforts"].as_array().unwrap().len(),
            7
        );
        assert_eq!(model["_meta"]["reasoningEfforts"][0]["id"], "off");
        assert_eq!(model["_meta"]["reasoningEfforts"][0]["value"], "none");
        assert_eq!(model["_meta"]["reasoningEffort"], "minimal");
        assert_eq!(model["_meta"]["edenThinking"]["effective"], "high");
    }
}
