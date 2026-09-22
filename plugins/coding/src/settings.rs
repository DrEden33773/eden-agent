//! The coding package's explicit configuration, with its documented defaults.
use super::*;
use std::sync::{Mutex, MutexGuard};

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
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
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
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
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
