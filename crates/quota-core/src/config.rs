//! Startup-only configuration for quota providers.
//!
//! The wire module owns JSONC file loading. This crate only defines the plain,
//! subc-independent configuration passed into [`crate::Registry`].

use serde::{Deserialize, Deserializer};

/// Longest useful reset-expiry threshold: banked credits expire after 30 days.
pub const MAX_AUTO_USE_RESETS_SECS: u64 = 30 * 24 * 60 * 60;

/// Configuration for all providers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub codex: CodexConfig,
}

/// Codex banked-reset policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// Seconds before credit expiry at which a used window may consume a reset.
    /// Zero disables both mutation and relaxed reporting.
    #[serde(default, deserialize_with = "deserialize_auto_use_resets")]
    pub auto_use_resets: u64,
}

impl CodexConfig {
    pub fn is_enabled(&self) -> bool {
        self.auto_use_resets > 0
    }
}

fn deserialize_auto_use_resets<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let seconds = value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| seconds as u64)
    });
    Ok(seconds.unwrap_or_default().min(MAX_AUTO_USE_RESETS_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> QuotaConfig {
        serde_json::from_str(input).unwrap()
    }

    #[test]
    fn auto_use_resets_validates_and_clamps_without_rejecting_the_file() {
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":86400}}"#)
                .codex
                .auto_use_resets,
            86_400
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":-1}}"#)
                .codex
                .auto_use_resets,
            0
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":"soon"}}"#)
                .codex
                .auto_use_resets,
            0
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":999999999}}"#)
                .codex
                .auto_use_resets,
            MAX_AUTO_USE_RESETS_SECS
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":1e30}}"#)
                .codex
                .auto_use_resets,
            MAX_AUTO_USE_RESETS_SECS
        );
    }

    #[test]
    fn absent_and_unknown_fields_keep_the_feature_off() {
        assert_eq!(parse("{}").codex, CodexConfig::default());
        assert_eq!(
            parse(r#"{"unknown":true,"codex":{"future_option":1}}"#).codex,
            CodexConfig::default()
        );
    }
}
