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
//! nextRefill JSON: `next` is the reset when present. CodexBar also models
//! `amount` and `duration` on this object, both optional -- transcribed here as
//! fact once, and it is NOT fact: no payload observed on any host has carried
//! them, and the one readable here (an account with no active AI quota) has
//! `{ "exception", "previous", "type" }`, two of which neither implementation
//! models. Treat the shape below as what we PARSE, never as what JetBrains
//! sends. A comment describing someone else's payload ages with no signal, and
//! this one was read back as evidence in a wire-design argument. `next` is the
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

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "jetbrains";

/// JetBrains config base dirs to scan for installed IDEs.
///
/// JetBrains is the one third-party source here that DOES follow host
/// convention, so it is the one that needs a Windows branch. The others read by
/// this module -- Codex, Gemini, OpenCode, Kilo, Codebuff -- are Node CLIs built
/// on `os.homedir()` and keep their POSIX-shaped paths on Windows, so mapping
/// their `~/.config` to `%APPDATA%` would break five sources to fix one. This is
/// a native application and stores under `%APPDATA%\JetBrains` there.
///
/// Every candidate is probed rather than selected by `cfg`: a path that does not
/// exist costs one failed `read_dir`, and probing all of them means a host with
/// an unusual layout is still found. The failure this avoids is the quiet one --
/// on Windows, scanning only the two Unix paths finds nothing, reports no active
/// quota, and is indistinguishable from an IDE that is genuinely not installed.
fn config_base_dirs() -> Vec<PathBuf> {
    config_base_dirs_from(crate::env::home_dir(), |key| std::env::var_os(key))
}

/// The candidate list, over an arbitrary environment.
///
/// Split from [`config_base_dirs`] for the same reason `env::home_dir_from`
/// exists: reading the process environment inside the function leaves the
/// Windows branch exercisable only on Windows, and a branch that can only be
/// tested where it runs is one nobody checks until a user reports that nothing
/// resolves. Here that report would never come -- the provider degrades as "no
/// active quota", which is the same thing it says on a host with no IDE.
fn config_base_dirs_from(
    home: Option<PathBuf>,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join("Library/Application Support/JetBrains")); // macOS
        dirs.push(home.join(".config/JetBrains")); // Linux
    }
    if let Some(xdg) = lookup("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(xdg).join("JetBrains"));
    }
    // Windows: the roaming application-data directory. `APPDATA` is set by the
    // OS; the `USERPROFILE` fallback covers a stripped environment, where the
    // literal `AppData\Roaming` is the value `APPDATA` would have held.
    if let Some(appdata) = lookup("APPDATA").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(appdata).join("JetBrains"));
    } else if let Some(profile) = lookup("USERPROFILE").filter(|v| !v.is_empty()) {
        dirs.push(
            PathBuf::from(profile)
                .join("AppData")
                .join("Roaming")
                .join("JetBrains"),
        );
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

    // type Unknown/Error (or absent current/maximum) means the IDE is installed
    // and its config was read, but this account has no AI quota to report. The
    // credential is fine and nothing is broken, so this is neither an absent
    // credential nor a failure -- a consumer must not count it as something to
    // fix, or the number never reaches zero.
    let used =
        used_percent(quota.current.as_deref(), quota.maximum.as_deref()).ok_or_else(|| {
            FetchError::NoQuotaReported(format!(
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

    // The same two figures the percent is computed from, published as absolute
    // counts. JetBrains states quota in units rather than requests, and a
    // consumer asking "how many units are left" would otherwise have to multiply
    // a percentage by a total it was never given.
    //
    // Whole numbers only, via the shared rule: an observed payload carries
    // fractional values such as "8134.155", and a fractional count is not the
    // count it appears to be.
    let (used_count, total_count) = quota
        .current
        .as_deref()
        .and_then(|c| c.trim().parse::<f64>().ok())
        .zip(
            quota
                .maximum
                .as_deref()
                .and_then(|m| m.trim().parse::<f64>().ok()),
        )
        .map_or((None, None), |(current, maximum)| {
            crate::model::window_counts(current, maximum)
        });

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent: used,
            raw_used_percent: None,
            resets_at: Some(resets_at),
            // NOT derived from `nextRefill.tariff.duration`, though that states
            // PT720H. The observed payload decomposes the balance into a tariff
            // pool that refills on that period and a purchased top-up pool that
            // does not, while `maximum` -- the percent's denominator -- is their
            // sum. One window length would claim the whole balance resets
            // monthly when part of it never does. See insula#1 for the capture.
            window_minutes: None,
            used_count,
            total_count,
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let path = discover_quota_file().ok_or_else(|| {
                FetchError::NoSession("no JetBrains AIAssistantQuotaManager2.xml found".to_string())
            })?;
            let bytes = crate::env::read_credential_file(&path, "JetBrains quota XML")?;
            let usage = normalize_usage(&bytes)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {

    /// The first OBSERVED JetBrains payload, end to end through the normalizer.
    ///
    /// LIVE CAPTURE, not synthetic: posted on insula#1 on 2026-08-17 by a
    /// credentialed account on another host, scrubbed with key names and value
    /// shapes intact, numbers rounded, timestamps shifted, no keys dropped.
    ///
    /// Every other fixture in this file was transcribed from a reference
    /// implementation, and one of those transcriptions was wrong -- it modelled
    /// `amount`/`duration` fields as fact when no payload here had ever shown
    /// them. This one is the reason that provenance distinction is annotated:
    /// the same bytes look identical whether observed or invented.
    ///
    /// Pinning the ARITHMETIC, which was never checkable before. `current` is
    /// the amount USED and `available` the remainder, proved by the payload's
    /// own decomposition: tariff 8100 used + 991900 available = 1000000 maximum.
    /// So the percent is 8100/1207000, and a reading of `current` as "remaining"
    /// -- the plausible misreading -- would report 99.3% used on an account that
    /// has spent two thirds of one percent.
    #[test]
    fn the_observed_credentialed_payload_normalizes_as_measured() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","until":"2026-11-14T21:00:00Z","tariffQuota":{"current":"8100.000","maximum":"1000000","available":"991900.000"},"topUpQuota":{"current":"0","maximum":"207000.000","available":"207000.000"}}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"PT720H"}}"#;
        let escape = |raw: &str| {
            raw.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
        };
        let xml = format!(
            r#"<application><component name="AIAssistantQuotaManager2"><option name="quotaInfo" value="{}" /><option name="nextRefill" value="{}" /></component></application>"#,
            escape(quota_info),
            escape(next_refill)
        );

        let usage = normalize_usage(xml.as_bytes()).expect("the observed payload must normalize");
        let window = usage.primary.expect("a primary window");

        assert!(
            (window.used_percent - 0.6711).abs() < 0.001,
            "expected 8100/1207000 = 0.6711%, got {}",
            window.used_percent
        );
        assert_eq!(window.used_count, Some(8100.0));
        assert_eq!(window.total_count, Some(1_207_000.0));
        assert_eq!(
            window.resets_at.as_deref(),
            Some("2026-07-15T06:00:00.000Z")
        );
        // Deliberately absent: the tariff period refills only part of the
        // balance the percent is measured against.
        assert_eq!(window.window_minutes, None);
    }

    /// A fractional unit balance publishes the percent and no counts.
    ///
    /// The observed capture was rounded for posting, but the reporter stated the
    /// real values are fractional (`"8134.155"`). The percent stays computable
    /// from fractions; the counts must not be invented from them.
    #[test]
    fn a_fractional_unit_balance_publishes_no_counts() {
        let quota_info = r#"{"type":"Available","current":"8134.155","maximum":"1207000.000"}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z"}"#;
        let escape = |raw: &str| {
            raw.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
        };
        let xml = format!(
            r#"<application><component name="AIAssistantQuotaManager2"><option name="quotaInfo" value="{}" /><option name="nextRefill" value="{}" /></component></application>"#,
            escape(quota_info),
            escape(next_refill)
        );

        let window = normalize_usage(xml.as_bytes())
            .expect("a fractional balance is still a valid quota")
            .primary
            .expect("a primary window");

        assert!(window.used_percent > 0.0, "the percent stays computable");
        assert_eq!(window.used_count, None, "a fractional count is not a count");
        assert_eq!(window.total_count, None, "both or neither");
    }

    /// The degraded refill payload this host actually writes yields no reset.
    ///
    /// LIVE CAPTURE, 2026-08-15, DataGrip 2026.2 on an account with no active AI
    /// quota: `nextRefill` carries `{"exception","previous","type"}`. No `next`,
    /// and two keys neither this module nor CodexBar models.
    ///
    /// Pinned because this file's header once described the object as
    /// `{type, next, amount, duration}` -- transcribed from CodexBar's optional
    /// model, then read back in a wire-design argument as though it described
    /// what JetBrains sends. This fixture is the only shape anyone here has
    /// actually observed.
    #[test]
    fn the_observed_degraded_refill_payload_yields_no_reset() {
        let refill: super::NextRefill = serde_json::from_str(
            r#"{"type":"Error","exception":"quota unavailable","previous":null}"#,
        )
        .expect(
            "unmodelled keys must not fail the parse: this upstream sends fields we do not model",
        );

        assert!(
            refill.next.is_none(),
            "this payload states no refill time, and inventing one would publish a \
             reset the upstream never gave"
        );
    }

    /// The Windows roaming directory is a candidate, and only there.
    ///
    /// JetBrains is the one third-party source here that follows host
    /// convention, so this branch is the difference between finding a live
    /// quota file on Windows and reporting "no active quota" -- which is what a
    /// host with no IDE installed also reports, so the failure carries no
    /// signal of its own.
    #[test]
    fn the_windows_roaming_directory_is_searched() {
        let env = |key: &str| match key {
            "APPDATA" => Some(std::ffi::OsString::from(r"C:\Users\qta\AppData\Roaming")),
            _ => None,
        };
        let dirs = config_base_dirs_from(Some(PathBuf::from(r"C:\Users\qta")), env);

        assert!(
            dirs.iter()
                .any(|d| d.ends_with("JetBrains") && d.to_string_lossy().contains("AppData")),
            "no roaming candidate in {dirs:?}"
        );
    }

    /// A stripped environment still reaches the roaming directory.
    ///
    /// `APPDATA` is normally set by the OS, so the fallback exists for a
    /// service-style environment where it is not. It reconstructs the literal
    /// value `APPDATA` would have held rather than guessing a different layout.
    #[test]
    fn a_missing_appdata_falls_back_to_the_profile() {
        let env = |key: &str| match key {
            "USERPROFILE" => Some(std::ffi::OsString::from(r"C:\Users\qta")),
            _ => None,
        };
        let dirs = config_base_dirs_from(None, env);

        let found = dirs.iter().find(|d| d.ends_with("JetBrains"));
        let found = found.expect("no candidate built from USERPROFILE");
        let shown = found.to_string_lossy().replace('\\', "/");
        assert!(
            shown.ends_with("AppData/Roaming/JetBrains"),
            "fallback built the wrong shape: {shown}"
        );
    }

    /// `APPDATA` wins when both are present, rather than both being added.
    ///
    /// The two describe the same directory, so emitting both would search it
    /// twice -- harmless but misleading to anyone reading the candidate list to
    /// understand where this looks.
    #[test]
    fn appdata_is_preferred_over_the_profile_fallback() {
        let env = |key: &str| match key {
            "APPDATA" => Some(std::ffi::OsString::from(r"D:\roaming")),
            "USERPROFILE" => Some(std::ffi::OsString::from(r"C:\Users\qta")),
            _ => None,
        };
        let dirs = config_base_dirs_from(None, env);

        let windows: Vec<_> = dirs.iter().filter(|d| d.ends_with("JetBrains")).collect();
        assert_eq!(windows.len(), 1, "expected one windows candidate: {dirs:?}");
        assert!(
            windows[0].to_string_lossy().starts_with("D:"),
            "{windows:?}"
        );
    }

    /// The Unix candidates survive the Windows branch being added.
    ///
    /// The regression this pins is a Windows fix written as a `cfg` swap rather
    /// than an addition, which would take macOS and Linux dark to light up a
    /// platform nobody here runs.
    #[test]
    fn the_unix_candidates_are_still_searched() {
        let dirs = config_base_dirs_from(Some(PathBuf::from("/home/qta")), |_| None);
        let shown: Vec<_> = dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();

        assert!(
            shown
                .iter()
                .any(|d| d.contains("Library/Application Support/JetBrains")),
            "macOS candidate missing: {shown:?}"
        );
        assert!(
            shown.iter().any(|d| d.ends_with(".config/JetBrains")),
            "Linux candidate missing: {shown:?}"
        );
    }

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

    /// The live state on a machine whose JetBrains IDE has no AI quota: the
    /// config was read and the account simply has nothing to report. Nothing is
    /// broken and nothing is fixable, so this must not be classed with the
    /// failures a user is expected to act on -- a permanent entry in that
    /// bucket would keep the count above zero when nothing is wrong.
    #[test]
    fn an_account_with_no_ai_quota_reports_no_quota_rather_than_a_failure() {
        let error =
            normalize_usage(UNKNOWN_XML.as_bytes()).expect_err("no quota means no usage windows");
        assert!(matches!(error, FetchError::NoQuotaReported(_)), "{error:?}");
        assert_eq!(error.error_class(), "no_quota_reported");
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
