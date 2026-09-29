//! Exact provider/model keys avoid ambiguous overrides across catalogs.
use eden_protocol::models::ModelTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Partial overrides inherit each absent value independently from the global settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)] // Fields name the two existing compaction controls.
pub struct BudgetOverride {
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
}

/// Resolution returns provenance per field so a settings view cannot imply a full override.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)] // Variant names are the complete source vocabulary.
pub enum BudgetSource {
    Global,
    Model { provider: String, model: String },
}

/// Each resolved value retains the scope that supplied it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct BudgetValue {
    pub tokens: u64,
    pub source: BudgetSource,
}

/// A run freezes this result; callers resolve again only at a safe run boundary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct EffectiveBudget {
    pub reserve_tokens: BudgetValue,
    pub keep_recent_tokens: BudgetValue,
}

/// Nested keys preserve provider and model identities without delimiter escaping.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct BudgetSettings {
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    #[serde(default)]
    pub models: BTreeMap<String, BTreeMap<String, BudgetOverride>>,
}

impl BudgetSettings {
    /// Zero budgets are rejected, including explicit overrides, rather than silently inherited.
    pub fn resolve(&self, target: &ModelTarget) -> Result<EffectiveBudget, crate::InputError> {
        let overrides = self
            .models
            .get(&target.provider)
            .and_then(|models| models.get(&target.model));
        let resolve = |value: Option<u64>, fallback: u64| {
            let tokens = value.unwrap_or(fallback);
            if tokens == 0 {
                return Err(crate::InputError::InvalidLimit(
                    "compaction tokens must be positive".into(),
                ));
            }
            Ok(BudgetValue {
                tokens,
                source: if value.is_some() {
                    BudgetSource::Model {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                    }
                } else {
                    BudgetSource::Global
                },
            })
        };
        Ok(EffectiveBudget {
            reserve_tokens: resolve(
                overrides.and_then(|v| v.reserve_tokens),
                self.reserve_tokens,
            )?,
            keep_recent_tokens: resolve(
                overrides.and_then(|v| v.keep_recent_tokens),
                self.keep_recent_tokens,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_override_has_independent_sources_and_exact_identity() {
        let settings = BudgetSettings {
            reserve_tokens: 100,
            keep_recent_tokens: 200,
            models: BTreeMap::from([(
                "provider".into(),
                BTreeMap::from([(
                    "model".into(),
                    BudgetOverride {
                        reserve_tokens: Some(300),
                        keep_recent_tokens: None,
                    },
                )]),
            )]),
        };
        let mut target = ModelTarget {
            provider: "provider".into(),
            model: "model".into(),
            ..Default::default()
        };
        let effective = settings.resolve(&target).unwrap();
        assert_eq!(effective.reserve_tokens.tokens, 300);
        assert!(matches!(
            effective.reserve_tokens.source,
            BudgetSource::Model { .. }
        ));
        assert_eq!(
            effective.keep_recent_tokens,
            BudgetValue {
                tokens: 200,
                source: BudgetSource::Global
            }
        );
        target.provider = "other".into();
        assert_eq!(
            settings.resolve(&target).unwrap().reserve_tokens.tokens,
            100
        );
    }
}
