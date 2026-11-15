//! Delay policy shared by foreground aggregation retries and background probes.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDelayMode {
    #[default]
    Fixed,
    ExponentialJitter,
}

impl RetryDelayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::ExponentialJitter => "exponential_jitter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryDelayConfig {
    pub mode: RetryDelayMode,
    /// Upper bound for the first retry's full-jitter window.
    pub initial_interval_seconds: u32,
    pub max_interval_seconds: u32,
}

impl Default for RetryDelayConfig {
    fn default() -> Self {
        Self {
            mode: RetryDelayMode::Fixed,
            initial_interval_seconds: 1,
            max_interval_seconds: 60,
        }
    }
}

impl RetryDelayConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=86_400).contains(&self.initial_interval_seconds)
            || !(self.initial_interval_seconds..=86_400).contains(&self.max_interval_seconds)
        {
            return Err("退避初始间隔和最大间隔须为 1–86400 秒的整数，且最大间隔不小于初始间隔 (Backoff intervals must be 1–86400 seconds, with maximum >= initial)".into());
        }
        Ok(())
    }

    /// `failures` counts consecutive failures of this lane. Every caller samples
    /// separately, so parallel retries do not share a synchronized wake-up time.
    /// A success resets the lane to zero; fixed mode preserves zero-second waits.
    pub fn delay(&self, fixed_interval_seconds: u32, failures: u32) -> Duration {
        if self.mode == RetryDelayMode::Fixed {
            return Duration::from_secs(u64::from(fixed_interval_seconds));
        }
        let multiplier = 1u32 << failures.saturating_sub(1).min(31);
        let ceiling_seconds = self.initial_interval_seconds
            .saturating_mul(multiplier)
            .min(self.max_interval_seconds);
        let ceiling_ms = u64::from(ceiling_seconds) * 1000;
        // Reuse the existing UUID entropy source, taking bytes outside its fixed
        // version/variant bits. Multiply-high maps to the inclusive jitter range.
        let random_id = uuid::Uuid::new_v4();
        let bytes = random_id.as_bytes();
        let random = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let delay_ms = (u64::from(random) * (ceiling_ms + 1)) >> 32;
        Duration::from_millis(delay_ms)
    }
}
