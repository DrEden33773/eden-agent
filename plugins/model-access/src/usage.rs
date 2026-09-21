//! Accounting retains the wire payload and distinguishes absent counters from zero.
use eden_protocol::models::ModelTarget;
use serde_json::{Value, json};
pub(crate) fn normalize(raw: &Value, target: &ModelTarget, stop: Option<&str>) -> Value {
    let original = raw;
    let mapped;
    let raw = if matches!(
        target.api.as_str(),
        "google-generative-ai" | "google-vertex"
    ) {
        mapped = json!({
            "prompt_tokens": raw["promptTokenCount"],
            "completion_tokens": raw["candidatesTokenCount"]
                .as_u64()
                .map(|n| n.saturating_add(raw["thoughtsTokenCount"].as_u64().unwrap_or(0))),
            "prompt_tokens_details": { "cached_tokens": raw["cachedContentTokenCount"] },
            "completion_tokens_details": { "reasoning_tokens": raw["thoughtsTokenCount"] },
            "total_tokens": raw["totalTokenCount"],
        });
        &mapped
    } else if target.api == "bedrock-converse-stream" {
        mapped = json!({
            "input_tokens": raw["inputTokens"],
            "output_tokens": raw["outputTokens"],
            "cache_read_input_tokens": raw["cacheReadInputTokens"],
            "cache_creation_input_tokens": raw["cacheWriteInputTokens"],
            "total_tokens": raw["totalTokens"],
        });
        &mapped
    } else {
        raw
    };
    let anthropic = matches!(
        target.api.as_str(),
        "anthropic-messages" | "bedrock-converse-stream"
    );
    let input = raw[if anthropic
        || matches!(
            target.api.as_str(),
            "openai-responses" | "azure-openai-responses"
        ) {
        "input_tokens"
    } else {
        "prompt_tokens"
    }]
    .as_u64();
    let output = raw[if anthropic
        || matches!(
            target.api.as_str(),
            "openai-responses" | "azure-openai-responses"
        ) {
        "output_tokens"
    } else {
        "completion_tokens"
    }]
    .as_u64();
    let cache_read = if anthropic {
        raw["cache_read_input_tokens"].as_u64()
    } else {
        raw["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| raw["input_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| raw["prompt_cache_hit_tokens"].as_u64())
            .or_else(|| raw["num_cached_tokens"].as_u64())
            .or_else(|| raw["prompt_token_details"]["cached_tokens"].as_u64())
    };
    let cache_write = raw["cache_creation_input_tokens"].as_u64();
    let uncached = if anthropic {
        input
    } else {
        input.map(|v| v.saturating_sub(cache_read.unwrap_or(0)))
    };
    let reasoning = raw["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .or_else(|| raw["output_tokens_details"]["reasoning_tokens"].as_u64());
    let priced_input = input.map(|n| {
        n.saturating_add(if anthropic {
            cache_read
                .unwrap_or(0)
                .saturating_add(cache_write.unwrap_or(0))
        } else {
            0
        })
    });
    let tier = target.pricing.as_ref().and_then(|p| {
        p.tiers
            .iter()
            .filter(|tier| priced_input.is_some_and(|n| n > tier.input_tokens_above))
            .max_by_key(|tier| tier.input_tokens_above)
    });
    let cost = target.pricing.as_ref().and_then(|p| {
        let input_rate = tier.and_then(|t| t.input).or(p.input)?;
        let output_rate = tier.and_then(|t| t.output).or(p.output)?;
        let total = uncached? as f64 * input_rate + output? as f64 * output_rate;
        let read = match cache_read {
            Some(0) | None => 0.0,
            Some(n) => n as f64 * tier.and_then(|t| t.cache_read).or(p.cache_read)?,
        };
        let write = match cache_write {
            Some(0) | None => 0.0,
            Some(n) => n as f64 * tier.and_then(|t| t.cache_write).or(p.cache_write)?,
        };
        Some((total + read + write) / 1_000_000.0)
    });
    let total = raw["total_tokens"].as_u64().or_else(|| {
        input.zip(output).map(|(i, o)| {
            i.saturating_add(o).saturating_add(if anthropic {
                cache_read
                    .unwrap_or(0)
                    .saturating_add(cache_write.unwrap_or(0))
            } else {
                0
            })
        })
    });
    json!({
        "total_tokens": total,
        "raw": original,
        "normalized": {
            "input_tokens": uncached,
            "output_tokens": output,
            "cache_read_tokens": cache_read,
            "cache_write_tokens": cache_write,
            "reasoning_tokens": reasoning,
        },
        "cost": {
            "estimated_usd": cost,
            "estimated": true,
            "tier": tier,
            "pricing": target.pricing,
            "source": target.source,
        },
        "stop_reason": stop,
        "target": { "provider": target.provider, "model": target.model, "api": target.api },
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_context_tier_counts_cached_input_and_preserves_threshold_boundary() {
        let mut target = crate::projection::test_target("anthropic-messages");
        target.pricing = Some(
            serde_json::from_value(json!({
                "input": 1.0,
                "output": 2.0,
                "cache_read": 0.1,
                "cache_write": 1.0,
                "source": "fixture",
                "tiers": [{
                    "input_tokens_above": 100,
                    "input": 3.0,
                    "output": 4.0,
                    "cache_read": 0.3,
                }],
            }))
            .unwrap(),
        );
        let at = normalize(
            &json!({ "input_tokens": 60, "cache_read_input_tokens": 40, "output_tokens": 10 }),
            &target,
            None,
        );
        assert!(at["cost"]["tier"].is_null());
        let over = normalize(
            &json!({ "input_tokens": 61, "cache_read_input_tokens": 40, "output_tokens": 10 }),
            &target,
            None,
        );
        assert_eq!(over["cost"]["tier"]["input_tokens_above"], 100);
        let actual = over["cost"]["estimated_usd"].as_f64().unwrap();
        assert!((actual - 0.000235).abs() < 1e-12);
    }
    #[test]
    fn cache_and_reasoning_tokens_are_not_counted_twice() {
        let mut target = crate::projection::test_target("openai-completions");
        target.pricing = Some(
            serde_json::from_value(json!({
                "input": 2.0,
                "output": 4.0,
                "cache_read": 1.0,
                "cache_write": null,
                "source": "fixture",
            }))
            .unwrap(),
        );
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 40 },
            "completion_tokens_details": { "reasoning_tokens": 10 },
        });
        let value = normalize(&raw, &target, Some("stop"));
        assert_eq!(value["normalized"]["input_tokens"], 60);
        assert_eq!(value["cost"]["estimated_usd"], 0.00024);
        assert_eq!(value["raw"], raw);
    }
    #[test]
    fn unknown_counts_are_not_zero_cost() {
        let target = crate::projection::test_target("anthropic-messages");
        let value = normalize(&json!({}), &target, None);
        assert!(value["normalized"]["input_tokens"].is_null());
        assert!(value["cost"]["estimated_usd"].is_null());
    }
}
