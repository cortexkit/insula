//! JetBrains AI Assistant usage — read from the IDE's own local config XML.
//!
//! No network: JetBrains IDEs persist the AI quota to
//! `<config>/<IDE>/options/AIAssistantQuotaManager2.xml`, where two `<option>`
//! values (`quotaInfo`, `nextRefill`) hold HTML-entity-encoded JSON. We discover
//! the most-recently-written such file across installed IDEs, extract + entity-decode
//! the two JSON blobs, and map quota usage + next refill to a window.
//!
//! quotaInfo JSON: `{ "type", "current", "maximum", "until", "tariffQuota": {
//! "available" } }` (numbers are STRINGS). usedPercent = current/maximum*100.
//! nextRefill JSON: `{ "type", "next", "amount", "duration" }` → `next` is the
//! reset. A `type` of `Unknown`/`Error` (no active AI quota) degrades to NoSession.
//!
//! VERIFICATION: HYBRID. The file-discovery + XML-extract + entity-decode + JSON
//! parse + Unknown→degrade path is LIVE-verified (this machine has real
//! AIAssistantQuotaManager2.xml files; they currently read `type:"Unknown"`, which
//! the provider degrades correctly). The active-window MAPPING (current/maximum→%,
//! nextRefill.next→reset) is fixture-verified (CodexBar-sourced) — no active
//! JetBrains AI quota on this machine to live-anchor a real window. Field names +
//! mapping ported from CodexBar
//! `Sources/CodexBarCore/Providers/JetBrains/JetBrainsStatusProbe.swift:9-31,60-68,
//! 212-283` (usedPercent=current/maximum*100, resetsAt=nextRefill.next, HTML-entity
//! set, ISO8601 date parse). Dependency-light: no XML/regex crate — value extraction
//! is a string scan (the real `"` delimiters are unambiguous because the inner JSON
//! quotes are `&quot;`), mirroring CodexBar's own regex-free Linux path.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "jetbrains";

/// JetBrains config base dirs to scan for installed IDEs (macOS + Linux/XDG).
fn config_base_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("Library/Application Support/JetBrains")); // macOS
        dirs.push(home.join(".config/JetBrains")); // Linux
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(xdg).join("JetBrains"));
    }
    dirs
}

const QUOTA_FILE_REL: &str = "options/AIAssistantQuotaManager2.xml";

/// Find the most-recently-modified AIAssistantQuotaManager2.xml across installed
/// IDEs (mtime as a proxy for the active IDE, matching CodexBar's "latest IDE").
fn discover_quota_file() -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for base in config_base_dirs() {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join(QUOTA_FILE_REL);
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, candidate));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Decode the small set of XML/HTML entities JetBrains writes (CodexBar `:212-220`).
fn decode_html_entities(s: &str) -> String {
    s.replace("&#10;", "\n")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Extract one `<option name="NAME" value="VALUE" />`'s raw value from the
/// AIAssistantQuotaManager2 component. The value's real `"` delimiters are
/// unambiguous: the inner JSON quotes are `&quot;`, so a plain scan is safe.
fn extract_option_value(xml: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let after_name = &xml[xml.find(&needle)? + needle.len()..];
    let value_key = "value=\"";
    let after_value = &after_name[after_name.find(value_key)? + value_key.len()..];
    let end = after_value.find('"')?;
    Some(after_value[..end].to_string())
}

#[derive(Debug, Deserialize)]
struct QuotaInfo {
    #[serde(rename = "type")]
    kind: Option<String>,
    current: Option<String>,
    maximum: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NextRefill {
    next: Option<String>,
}

/// Parse `current`/`maximum` (string numbers) → used percent. CodexBar `:24-25`.
fn used_percent(current: Option<&str>, maximum: Option<&str>) -> Option<f64> {
    let current: f64 = current?.trim().parse().ok()?;
    let maximum: f64 = maximum?.trim().parse().ok()?;
    if maximum <= 0.0 {
        return None;
    }
    Some(((current / maximum) * 100.0).clamp(0.0, 100.0))
}

/// Normalize the IDE quota XML to [`Usage`]. Pure — unit-testable against a
/// CodexBar-shaped fixture.
pub fn normalize_usage(xml_bytes: &[u8]) -> Result<Usage, FetchError> {
    let xml = std::str::from_utf8(xml_bytes)
        .map_err(|e| FetchError::Decode(format!("jetbrains xml not UTF-8: {e}")))?;

    let quota_raw = extract_option_value(xml, "quotaInfo")
        .ok_or_else(|| FetchError::NoSession("jetbrains: no quotaInfo in config".to_string()))?;
    let quota: QuotaInfo = serde_json::from_str(&decode_html_entities(&quota_raw))
        .map_err(|e| FetchError::Decode(format!("jetbrains quotaInfo not JSON: {e}")))?;

    // type Unknown/Error (or absent current/maximum) = no active AI quota — a normal
    // not-configured state, surfaced as NoSession (folds into silent-degrade).
    let used =
        used_percent(quota.current.as_deref(), quota.maximum.as_deref()).ok_or_else(|| {
            FetchError::NoSession(format!(
                "jetbrains: no active quota (type {:?})",
                quota.kind.as_deref().unwrap_or("?")
            ))
        })?;

    let resets_at = extract_option_value(xml, "nextRefill")
        .and_then(|raw| serde_json::from_str::<NextRefill>(&decode_html_entities(&raw)).ok())
        .and_then(|r| r.next)
        .filter(|s| !s.trim().is_empty());

    // A quota with no real refill date is not a well-formed window.
    let resets_at = resets_at.ok_or_else(|| {
        FetchError::Decode("jetbrains: quota present but no refill date".to_string())
    })?;

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent: used,
            resets_at: Some(resets_at),
            window_minutes: None,
        }),
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The JetBrains usage provider.
pub struct JetBrainsProvider;

impl JetBrainsProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JetBrainsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for JetBrainsProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let path = discover_quota_file().ok_or_else(|| {
            FetchError::NoSession("no JetBrains AIAssistantQuotaManager2.xml found".to_string())
        })?;
        let bytes = std::fs::read(&path)
            .map_err(|e| FetchError::NoSession(format!("reading {}: {e}", path.display())))?;
        let usage = normalize_usage(&bytes)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CodexBar-shaped active quota: numbers are STRINGS, JSON is HTML-entity
    /// encoded inside the option value, dates are ISO8601.
    const ACTIVE_XML: &str = r#"<application>
  <component name="AIAssistantQuotaManager2">
    <option name="nextRefill" value="{&#10;    &quot;type&quot;: &quot;Available&quot;,&#10;    &quot;next&quot;: &quot;2026-07-01T00:00:00Z&quot;&#10;}" />
    <option name="quotaInfo" value="{&#10;    &quot;type&quot;: &quot;Ready&quot;,&#10;    &quot;current&quot;: &quot;250&quot;,&#10;    &quot;maximum&quot;: &quot;1000&quot;,&#10;    &quot;tariffQuota&quot;: { &quot;available&quot;: &quot;750&quot; }&#10;}" />
  </component>
</application>"#;

    /// The real shape this machine currently writes (no active AI quota).
    const UNKNOWN_XML: &str = r#"<application>
  <component name="AIAssistantQuotaManager2">
    <option name="nextRefill" value="{&#10;    &quot;type&quot;: &quot;Error&quot;&#10;}" />
    <option name="quotaInfo" value="{&#10;    &quot;type&quot;: &quot;Unknown&quot;&#10;}" />
  </component>
</application>"#;

    #[test]
    fn normalizes_active_quota() {
        let usage = normalize_usage(ACTIVE_XML.as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0); // 250/1000
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z"));
    }

    #[test]
    fn unknown_type_degrades_as_no_session() {
        // The live state on a machine without active JetBrains AI quota.
        assert!(matches!(
            normalize_usage(UNKNOWN_XML.as_bytes()),
            Err(FetchError::NoSession(_))
        ));
    }

    #[test]
    fn active_quota_without_refill_is_decode_error() {
        let xml = r#"<application><component name="AIAssistantQuotaManager2">
            <option name="quotaInfo" value="{&quot;type&quot;:&quot;Ready&quot;,&quot;current&quot;:&quot;1&quot;,&quot;maximum&quot;:&quot;10&quot;}" />
        </component></application>"#;
        assert!(matches!(
            normalize_usage(xml.as_bytes()),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn missing_component_degrades() {
        assert!(matches!(
            normalize_usage(b"<application></application>"),
            Err(FetchError::NoSession(_))
        ));
    }

    #[test]
    fn entity_decode_roundtrip() {
        assert_eq!(decode_html_entities("a&quot;b&#10;c"), "a\"b\nc");
    }
}
