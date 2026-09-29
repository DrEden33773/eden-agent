//! The coding package's explicit configuration, with its documented defaults.
use super::*;
use eden_model_input::{BudgetOverride, BudgetSettings, EffectiveBudget, ImageChoice, ImageLimits};
use eden_plugin_sdk::protocol::models::ModelTarget;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ImageOverride {
    pub mode: Option<ImageChoice>,
    pub limits: ImageLimits,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ImageSettings {
    pub mode: ImageChoice,
    pub limits: ImageLimits,
    pub models: BTreeMap<String, BTreeMap<String, ImageOverride>>,
}

#[derive(Debug)]
struct Controls {
    auto_compaction: bool,
    auto_retry: bool,
    waiting: usize,
    stop: tokio::sync::watch::Sender<u64>,
}

/// A registered wait is removed on every exit, including future cancellation.
pub(crate) struct RetryWait {
    controls: Arc<Mutex<Controls>>,
    stop: tokio::sync::watch::Receiver<u64>,
}
impl RetryWait {
    pub(crate) async fn stopped(&mut self) {
        let _ = self.stop.changed().await;
    }
}
impl Drop for RetryWait {
    fn drop(&mut self) {
        self.controls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .waiting -= 1;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    controls: Arc<Mutex<Controls>>,
    pub policies: Vec<eden_plugin_sdk::protocol::context_edit::Policy>,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    global_budgets: (u64, u64),
    pub model_budgets: BTreeMap<String, BTreeMap<String, BudgetOverride>>,
    pub effective_budget: Option<EffectiveBudget>,
    pub images: ImageSettings,
    pub image_mode: ImageChoice,
    pub image_limits: ImageLimits,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            controls: Arc::new(Mutex::new(Controls {
                auto_compaction: true,
                auto_retry: true,
                waiting: 0,
                stop: tokio::sync::watch::channel(0).0,
            })),
            policies: vec![eden_plugin_sdk::protocol::context_edit::Policy {
                name: "large-tool-output".into(),
                role: "large_tool_output".into(),
                boundary: eden_plugin_sdk::protocol::context_edit::Boundary::BeforeRequest,
                enabled: false,
                config: json!({
                    "threshold_chars": 16000,
                    "keep_chars": 4000,
                    "keep_recent_results": 2,
                }),
            }],
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            global_budgets: (16384, 20000),
            model_budgets: BTreeMap::new(),
            effective_budget: None,
            images: ImageSettings::default(),
            image_mode: ImageChoice::Auto,
            image_limits: ImageLimits::default(),
            max_retries: 3,
            base_delay_ms: 2000,
        }
    }
}
impl Settings {
    fn controls(&self) -> MutexGuard<'_, Controls> {
        self.controls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
    pub(crate) fn auto_compaction(&self) -> bool {
        self.controls().auto_compaction
    }
    pub(crate) fn control(&self, request: CodingControlRequest) -> CodingControlState {
        let mut controls = self.controls();
        match request {
            CodingControlRequest::Inspect => {}
            CodingControlRequest::SetAutoCompaction { enabled } => {
                controls.auto_compaction = enabled
            }
            CodingControlRequest::SetAutoRetry { enabled } => {
                controls.auto_retry = enabled;
                if !enabled && controls.waiting > 0 {
                    controls
                        .stop
                        .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
                }
            }
            CodingControlRequest::StopRetry => {
                if controls.waiting > 0 {
                    controls
                        .stop
                        .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
                }
            }
        }
        CodingControlState {
            auto_compaction: controls.auto_compaction,
            auto_retry: controls.auto_retry,
            retry_waiting: controls.waiting > 0,
        }
    }
    pub(crate) fn begin_retry(&self) -> Option<RetryWait> {
        let mut controls = self.controls();
        if !controls.auto_retry {
            return None;
        }
        controls.waiting += 1;
        Some(RetryWait {
            controls: self.controls.clone(),
            stop: controls.stop.subscribe(),
        })
    }

    pub(crate) fn parse(value: Value) -> Result<Self, Fault> {
        let mut settings = Self::default();
        if !value.is_null() && !value.is_object() {
            return Err(invalid("coding settings must be an object"));
        }
        if let Some(policies) = value.get("context_policies") {
            settings.policies = serde_json::from_value(policies.clone())
                .map_err(|error| invalid(error.to_string()))?;
            let mut names = BTreeSet::new();
            for policy in &settings.policies {
                if policy.name.trim().is_empty()
                    || policy.role.trim().is_empty()
                    || !names.insert(&policy.name)
                {
                    return Err(invalid(
                        "context policies require unique names and nonempty roles",
                    ));
                }
            }
        }
        for name in ["compaction", "retry"] {
            if let Some(section) = value.get(name)
                && !section.is_object()
            {
                return Err(invalid(format!("{name} settings must be an object")));
            }
        }
        if let Some(enabled) = value["compaction"].get("enabled") {
            settings.controls().auto_compaction = enabled
                .as_bool()
                .ok_or_else(|| invalid("compaction.enabled must be boolean"))?;
        }
        for (section, key, target) in [
            ("compaction", "reserve_tokens", &mut settings.reserve_tokens),
            (
                "compaction",
                "keep_recent_tokens",
                &mut settings.keep_recent_tokens,
            ),
            ("retry", "base_delay_ms", &mut settings.base_delay_ms),
        ] {
            if let Some(number) = value[section].get(key) {
                *target = number.as_u64().ok_or_else(|| {
                    invalid(format!("{section}.{key} must be an unsigned integer"))
                })?;
            }
        }
        if settings.reserve_tokens == 0 || settings.keep_recent_tokens == 0 {
            return Err(invalid("compaction token settings must be positive"));
        }
        if let Some(models) = value["compaction"].get("models") {
            settings.model_budgets = serde_json::from_value(models.clone())
                .map_err(|error| invalid(format!("compaction.models: {error}")))?;
            if settings
                .model_budgets
                .values()
                .flat_map(|models| models.values())
                .any(|budget| {
                    budget.reserve_tokens == Some(0) || budget.keep_recent_tokens == Some(0)
                })
            {
                return Err(invalid("compaction model token settings must be positive"));
            }
        }
        if let Some(images) = value.get("images") {
            settings.images = serde_json::from_value(images.clone())
                .map_err(|error| invalid(format!("images: {error}")))?;
            validate_image_setting(settings.images.mode, &settings.images.limits)?;
            for model in settings
                .images
                .models
                .values()
                .flat_map(|models| models.values())
            {
                validate_image_setting(model.mode.unwrap_or(settings.images.mode), &model.limits)?;
            }
        }
        settings.image_mode = settings.images.mode;
        settings.image_limits = settings.images.limits.clone();
        if let Some(enabled) = value["retry"].get("enabled") {
            settings.controls().auto_retry = enabled
                .as_bool()
                .ok_or_else(|| invalid("retry.enabled must be boolean"))?;
        }
        if let Some(retries) = value["retry"].get("max_retries") {
            settings.max_retries = retries
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| invalid("retry.max_retries must be a 32-bit unsigned integer"))?;
        }
        settings.global_budgets = (settings.reserve_tokens, settings.keep_recent_tokens);
        Ok(settings)
    }
    /// Freeze model overrides once per run while preserving the shared retry controls.
    pub(crate) fn for_target(&self, target: &Option<ModelTarget>) -> Result<Self, Fault> {
        let mut resolved = self.clone();
        let fallback = ModelTarget::default();
        let target_ref = target.as_ref().unwrap_or(&fallback);
        let budget = BudgetSettings {
            reserve_tokens: self.global_budgets.0,
            keep_recent_tokens: self.global_budgets.1,
            models: if target.is_some() {
                self.model_budgets.clone()
            } else {
                BTreeMap::new()
            },
        }
        .resolve(target_ref)
        .map_err(|error| invalid(error.to_string()))?;
        resolved.reserve_tokens = budget.reserve_tokens.tokens;
        resolved.keep_recent_tokens = budget.keep_recent_tokens.tokens;
        resolved.effective_budget = Some(budget);
        let overrides = target.as_ref().and_then(|target| {
            self.images
                .models
                .get(&target.provider)
                .and_then(|models| models.get(&target.model))
        });
        resolved.image_mode = overrides
            .and_then(|value| value.mode)
            .unwrap_or(self.images.mode);
        resolved.image_limits =
            merge_image_limits(&self.images.limits, overrides.map(|value| &value.limits));
        if let Some(actual) = target
            .as_ref()
            .and_then(|target| target.compat.get("image_limits"))
        {
            let actual: ImageLimits = serde_json::from_value(actual.clone())
                .map_err(|error| invalid(format!("model image_limits: {error}")))?;
            validate_image_setting(resolved.image_mode, &actual)?;
            // Configuration may tighten actual provider constraints, never relax them.
            constrain_image_limits(&mut resolved.image_limits, &actual);
        }
        Ok(resolved)
    }
    pub(crate) fn summary_allowance(&self, split: bool, model_max: u32) -> u32 {
        let fraction = if split { 5 } else { 8 };
        let allowance = self.reserve_tokens.saturating_mul(fraction) / 10;
        let allowance = u32::try_from(allowance).unwrap_or(u32::MAX).max(1);
        if model_max == 0 {
            allowance
        } else {
            allowance.min(model_max)
        }
    }
}
fn validate_image_setting(mode: ImageChoice, limits: &ImageLimits) -> Result<(), Fault> {
    if !matches!(mode, ImageChoice::Auto | ImageChoice::Preserve) {
        return Err(invalid(
            "images.mode must be auto or preserve; omission and re-adaptation require an explicit \
             image operation",
        ));
    }
    eden_model_input::validate_images(&[], &ModelTarget::default(), limits, None)
        .map_err(|error| invalid(error.to_string()))
}
fn merge_image_limits(global: &ImageLimits, model: Option<&ImageLimits>) -> ImageLimits {
    let Some(model) = model else {
        return global.clone();
    };
    ImageLimits {
        max_width: model.max_width.or(global.max_width),
        max_height: model.max_height.or(global.max_height),
        max_pixels: model.max_pixels.or(global.max_pixels),
        max_image_bytes: model.max_image_bytes.or(global.max_image_bytes),
        max_images: model.max_images.or(global.max_images),
        max_body_bytes: model.max_body_bytes.or(global.max_body_bytes),
    }
}
fn constrain_image_limits(config: &mut ImageLimits, actual: &ImageLimits) {
    fn bound<T: Ord + Copy>(config: Option<T>, actual: Option<T>) -> Option<T> {
        match (config, actual) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    config.max_width = bound(config.max_width, actual.max_width);
    config.max_height = bound(config.max_height, actual.max_height);
    config.max_pixels = bound(config.max_pixels, actual.max_pixels);
    config.max_image_bytes = bound(config.max_image_bytes, actual.max_image_bytes);
    config.max_images = bound(config.max_images, actual.max_images);
    config.max_body_bytes = bound(config.max_body_bytes, actual.max_body_bytes);
}
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "coding-settings", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_settings_inherit_per_field_and_cannot_relax_catalog_limits() {
        let settings = Settings::parse(json!({
            "compaction": {
                "reserve_tokens": 100,
                "models": { "p": { "m": { "reserve_tokens": 50 } } },
            },
            "images": {
                "limits": { "max_width": 800 },
                "models": { "p": { "m": { "mode": "preserve", "limits": { "max_height": 500 } } } },
            },
        }))
        .unwrap();
        let target = Some(ModelTarget {
            provider: "p".into(),
            model: "m".into(),
            compat: json!({ "image_limits": { "max_width": 400 } }),
            ..Default::default()
        });
        let selected = settings.for_target(&target).unwrap();
        assert_eq!(selected.reserve_tokens, 50);
        assert_eq!(selected.keep_recent_tokens, 20000);
        assert_eq!(selected.image_mode, ImageChoice::Preserve);
        assert_eq!(selected.image_limits.max_width, Some(400));
        assert_eq!(selected.image_limits.max_height, Some(500));
        assert_eq!(selected.for_target(&None).unwrap().reserve_tokens, 100);
        assert!(matches!(
            selected.effective_budget.unwrap().keep_recent_tokens.source,
            eden_model_input::BudgetSource::Global
        ));
    }
    #[test]
    fn invalid_model_budgets_and_implicit_omission_configuration_are_rejected() {
        assert!(
            Settings::parse(json!({
                "compaction": { "models": { "p": { "m": { "reserve_tokens": 0 } } } },
            }))
            .is_err()
        );
        assert!(Settings::parse(json!({ "images": { "mode": "omit" } })).is_err());
        assert!(Settings::parse(json!({ "images": { "limits": { "max_pixels": 0 } } })).is_err());
    }
    #[test]
    fn default_settings_and_explicit_small_retention_are_independent() {
        let defaults = Settings::parse(Value::Null).unwrap();
        assert!(defaults.auto_compaction());
        assert_eq!(defaults.keep_recent_tokens, 20000);
        assert_eq!(defaults.summary_allowance(false, 100000), 13107);
        assert_eq!(defaults.summary_allowance(true, 100000), 8192);
        let small = Settings::parse(json!({
            "compaction": { "enabled": false, "keep_recent_tokens": 1 },
            "retry": { "max_retries": 0, "base_delay_ms": 1 },
        }))
        .unwrap();
        assert!(!small.auto_compaction());
        assert_eq!(small.keep_recent_tokens, 1);
        assert_eq!(small.reserve_tokens, 16384);
        assert_eq!(small.max_retries, 0);
        assert_eq!(defaults.keep_recent_tokens, 20000);
    }
}
