use super::*;

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub compaction_enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            compaction_enabled: true,
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            max_retries: 3,
            base_delay_ms: 2000,
        }
    }
}
impl Settings {
    pub(crate) fn parse(value: Value) -> Result<Self, Fault> {
        let mut settings = Self::default();
        if !value.is_null() && !value.is_object() {
            return Err(invalid("coding settings must be an object"));
        }
        for name in ["compaction", "retry"] {
            if let Some(section) = value.get(name)
                && !section.is_object()
            {
                return Err(invalid(format!("{name} settings must be an object")));
            }
        }
        if let Some(enabled) = value["compaction"].get("enabled") {
            settings.compaction_enabled = enabled
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
        if let Some(retries) = value["retry"].get("max_retries") {
            settings.max_retries = retries
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| invalid("retry.max_retries must be a 32-bit unsigned integer"))?;
        }
        Ok(settings)
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
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "coding-settings", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_settings_and_explicit_small_retention_are_independent() {
        let defaults = Settings::parse(Value::Null).unwrap();
        assert!(defaults.compaction_enabled);
        assert_eq!(defaults.keep_recent_tokens, 20000);
        assert_eq!(defaults.summary_allowance(false, 100000), 13107);
        assert_eq!(defaults.summary_allowance(true, 100000), 8192);
        let small = Settings::parse(json!({
            "compaction": { "enabled": false, "keep_recent_tokens": 1 },
            "retry": { "max_retries": 0, "base_delay_ms": 1 },
        }))
        .unwrap();
        assert!(!small.compaction_enabled);
        assert_eq!(small.keep_recent_tokens, 1);
        assert_eq!(small.reserve_tokens, 16384);
        assert_eq!(small.max_retries, 0);
        assert_eq!(defaults.keep_recent_tokens, 20000);
    }
}
