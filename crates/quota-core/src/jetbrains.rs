//! JetBrains AI Assistant usage — read from the IDE's own local config XML.
//!
//! No network: JetBrains IDEs persist the AI quota to
//! `<config>/<IDE>/options/AIAssistantQuotaManager2.xml`, where two `<option>`
//! values (`quotaInfo`, `nextRefill`) hold HTML-entity-encoded JSON. We discover
//! the most-recently-written such file across installed IDEs, extract + entity-decode
//! the two JSON blobs, and map quota usage + next refill to a window.
//!
//! quotaInfo JSON: `{ "type", "current", "maximum", "until", "tariffQuota": {
//! "current", "maximum", "available" }, "topUpQuota": { same } }` (numbers are
//! STRINGS; shape from the insula#1 capture). The top-level figures are the sum
//! of the two parts, and only the tariff refills at `nextRefill`: the top-up is
//! purchased, does not renew, and is drawn after the tariff is spent.
//!
//! MAPPING. When `tariffQuota` is readable the primary window is the tariff
//! alone -- usedPercent = tariff.current/tariff.maximum*100, counts from the
//! tariff, and `windowMinutes` from `nextRefill.tariff.duration` when stated --
//! and `topUpQuota` is published as one `Purchased` spend pool (remaining =
//! `available`, total = `maximum`). A window over the sum could never reach
//! 100% while top-up remained, so it read as headroom once the refilling part
//! was exhausted. Without `tariffQuota` there is nothing to split: the window is
//! the summed current/maximum, with no `windowMinutes` and no pool.
//! nextRefill JSON: `next` is the reset when present. CodexBar also models
//! `amount` and `duration` on this object, both optional -- transcribed here as
//! fact once, and it is NOT fact: no payload observed on any host has carried
//! them, and the one readable here (an account with no active AI quota) has
//! `{ "exception", "previous", "type" }`, two of which neither implementation
//! models. Treat the shape below as what we PARSE, never as what JetBrains
//! sends. A comment describing someone else's payload ages with no signal, and
//! this one was read back as evidence in a wire-design argument. `next` is the
//! reset. A `type` of `Unknown`/`Error` (no active AI quota) degrades to
//! `NoQuotaReported`, NOT to `NoSession`. That distinction is the whole point of
//! the class: `NoSession` publishes `credential_absent`, which tells a consumer
//! nobody configured this provider and authorises pruning the account. Here the
//! IDE is installed and its config was read successfully -- the credential is
//! fine and the account simply has no AI quota, which is a state to report rather
//! than something to fix. This line said `NoSession` until 2026-09-20 and the code
//! has not agreed with it since the taxonomy split.
//!
//! VERIFICATION: HYBRID. The file-discovery + XML-extract + entity-decode + JSON
//! parse + Unknown→degrade path is LIVE-verified (this machine has real
//! AIAssistantQuotaManager2.xml files; they currently read `type:"Unknown"`, which
//! the provider degrades correctly). The active-window MAPPING (tariff split,
//! top-up pool, nextRefill.next→reset) is fixture-verified (the insula#1 capture
//! and CodexBar-sourced fixtures) — no active
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

use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    model::{
        Amount, Pool, PoolBasis, PoolFunding, ProviderUsage, RateWindow, Regeneration,
        RegenerationRate, Usage,
    },
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
    /// The whole balance: the tariff and the top-up summed. Read only when the
    /// payload carries no `tariffQuota` to split it by.
    current: Option<String>,
    maximum: Option<String>,
    /// The part of the balance that refills at `nextRefill`.
    #[serde(rename = "tariffQuota")]
    tariff_quota: Option<SubQuota>,
    /// The purchased part, which does not refill and is drawn only after the
    /// tariff is spent.
    #[serde(rename = "topUpQuota")]
    top_up_quota: Option<SubQuota>,
}

/// One part of the balance. Every figure is a decimal string (`"207000.000"`).
///
/// Observed on insula#1: `current` is the amount USED and `available` the
/// remainder, so `current + available == maximum` within each part.
#[derive(Debug, Deserialize)]
struct SubQuota {
    current: Option<String>,
    maximum: Option<String>,
    available: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NextRefill {
    next: Option<String>,
    /// The grant that arrives at `next`, when the payload states one.
    tariff: Option<Tariff>,
}

/// The recurring grant: how much arrives, and how often.
#[derive(Debug, Deserialize)]
struct Tariff {
    /// String-encoded like every other number in this payload (`"1000000"`).
    amount: Option<String>,
    /// ISO-8601 duration between grants. Observed: `"PT720H"`.
    duration: Option<String>,
}

/// Parse an ISO-8601 duration into whole minutes.
///
/// Deliberately narrow, and the narrowness is the point: this feeds a wire field
/// whose whole purpose is to carry a STATED mechanic, so a duration this parser
/// cannot read must yield nothing rather than a guess.
///
/// `M` IS AMBIGUOUS AND THAT IS THE TRAP. In the date part it means months, in
/// the time part minutes, and only position separates them -- `P1M` and `PT1M`
/// differ by a factor of about forty-four thousand. Months and years are refused
/// outright rather than approximated: a month is not a fixed number of minutes,
/// and picking 30 days would publish a rate the upstream never stated.
///
/// Seconds are accepted and truncated toward zero, so a sub-minute period yields
/// 0 and is refused by the caller. A rate of "some amount per zero minutes" is
/// not a rate.
fn iso8601_duration_minutes(raw: &str) -> Option<i64> {
    let text = raw.trim();
    let body = text.strip_prefix('P')?;
    // Split at the time designator. Before it, only days are admissible; after
    // it, hours/minutes/seconds.
    let (date_part, time_part) = match body.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (body, None),
    };

    let mut minutes: i64 = 0;
    let mut number = String::new();

    for ch in date_part.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let value: i64 = number.parse().ok()?;
        number.clear();
        match ch {
            'D' => minutes = minutes.checked_add(value.checked_mul(1440)?)?,
            'W' => minutes = minutes.checked_add(value.checked_mul(10080)?)?,
            // Years and months have no fixed length in minutes.
            _ => return None,
        }
    }
    if !number.is_empty() {
        // Trailing digits with no unit: malformed.
        return None;
    }

    if let Some(time_part) = time_part {
        for ch in time_part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
                continue;
            }
            match ch {
                'H' => {
                    let value: i64 = number.parse().ok()?;
                    minutes = minutes.checked_add(value.checked_mul(60)?)?;
                }
                'M' => {
                    let value: i64 = number.parse().ok()?;
                    minutes = minutes.checked_add(value)?;
                }
                'S' => {
                    let value: f64 = number.parse().ok()?;
                    minutes = minutes.checked_add((value / 60.0) as i64)?;
                }
                _ => return None,
            }
            number.clear();
        }
        if !number.is_empty() {
            return None;
        }
    }

    Some(minutes)
}

/// Build the stated replenishment mechanic from `nextRefill`.
///
/// EMITTED ONLY WHEN THE PAYLOAD STATES AN AMOUNT, which is what separates this
/// from the anthropic decision six days earlier: an upstream that gives only a
/// reset instant states WHEN quota returns and nothing about HOW, and `resets_at`
/// already carries that. Stamping `cliff` on a bare instant would mint an
/// upstream statement out of our own reading.
///
/// The `tariff` block is what makes this different. A named amount arriving at a
/// discrete `next` instant is a lump, and `cliff` is precisely the claim that
/// accrual before that instant is zero -- which the payload supports by giving
/// the whole grant one arrival time.
///
/// An unparseable or absent `duration` keeps the mechanic and drops the rate. The
/// shipped type documents that shape as real and common: a mechanic described
/// without a quantity.
fn regeneration_from(refill: &NextRefill) -> Option<Regeneration> {
    let tariff = refill.tariff.as_ref()?;
    let amount: f64 = tariff.amount.as_deref()?.trim().parse().ok()?;
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }

    let rate = tariff
        .duration
        .as_deref()
        .and_then(iso8601_duration_minutes)
        .filter(|minutes| *minutes > 0)
        .map(|per_minutes| RegenerationRate {
            amount,
            per_minutes,
        });

    Some(Regeneration {
        mechanic: "cliff".to_string(),
        rate,
    })
}

/// What JetBrains' quota is denominated in. The payload states no unit at all,
/// so this is a label for the provider's own quota units rather than a currency:
/// a consumer must not read these amounts as money.
const TOP_UP_UNIT: &str = "jetbrains-ai-quota";

/// Build the spend pool for the purchased top-up, from `topUpQuota`.
///
/// The top-up is a balance drawn after the refilling tariff runs out, the same
/// mechanic as Codex credits or Claude's extra usage, so it is published as a
/// pool beside the window rather than folded into the window's percent.
///
/// Refused -- `None`, nothing logged -- when either figure is absent or not
/// exactly a decimal number. A pool with a guessed balance is worse than no
/// pool, since a consumer spends against the figure it is given.
fn top_up_pool(top_up: &SubQuota) -> Option<Pool> {
    let remaining = parse_amount(top_up.available.as_deref()?, TOP_UP_UNIT)?;
    let total = parse_amount(top_up.maximum.as_deref()?, TOP_UP_UNIT)?;
    let (remaining, total) = same_exponent(remaining, total)?;
    Some(Pool {
        // The provider's own key for this part of the balance.
        id: "topUpQuota".to_string(),
        label: "Top-up quota".to_string(),
        // A top-up is bought, and `Purchased` is documented as exactly that:
        // spending it costs money. It never renews, so `Subscription` would be
        // wrong, and nothing in the payload suggests it could be a free grant.
        funding: PoolFunding::Purchased,
        remaining: Some(remaining),
        total: Some(total),
        // `available` is the provider's own statement of what is left of this
        // pool alone, not a share computed from a figure covering several.
        basis: PoolBasis::Reported,
        // The payload has no enable flag for the top-up.
        spendable: None,
        // The top-up does not refill; `nextRefill` belongs to the tariff.
        resets_at: None,
    })
}

/// Restate two amounts at the larger of their exponents, so a pool's remaining
/// and total can be compared. Scaling up by a power of ten is exact; an overflow
/// refuses the pair rather than publishing a wrong figure.
fn same_exponent(a: Amount, b: Amount) -> Option<(Amount, Amount)> {
    let exponent = a.exponent.max(b.exponent);
    let rescale = |amount: Amount| -> Option<Amount> {
        let factor = 10i64.checked_pow(u32::from(exponent - amount.exponent))?;
        Some(Amount {
            minor: amount.minor.checked_mul(factor)?,
            exponent,
            unit: amount.unit,
        })
    };
    Some((rescale(a)?, rescale(b)?))
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
/// CodexBar-shaped fixture. The top-up pool, when there is one, is dropped here;
/// [`normalize`] returns both.
pub fn normalize_usage(xml_bytes: &[u8]) -> Result<Usage, FetchError> {
    normalize(xml_bytes).map(|normalized| normalized.usage)
}

/// The window and, when the payload splits the balance, the top-up pool.
#[derive(Debug)]
pub struct Normalized {
    pub usage: Usage,
    pub top_up: Option<Pool>,
}

/// Normalize the IDE quota XML to the window plus the top-up pool.
pub fn normalize(xml_bytes: &[u8]) -> Result<Normalized, FetchError> {
    let xml = std::str::from_utf8(xml_bytes)
        .map_err(|e| FetchError::Decode(format!("jetbrains xml not UTF-8: {e}")))?;

    let quota_raw = extract_option_value(xml, "quotaInfo")
        .ok_or_else(|| FetchError::NoSession("jetbrains: no quotaInfo in config".to_string()))?;
    let quota: QuotaInfo = serde_json::from_str(&decode_html_entities(&quota_raw))
        .map_err(|e| FetchError::Decode(format!("jetbrains quotaInfo not JSON: {e}")))?;

    // The figures the window is measured against. When the payload splits the
    // balance, the window is the tariff alone: it is the only part that refills
    // at `nextRefill`, so it is the only part a window's percent and length can
    // honestly describe. Without a readable split there is nothing to separate,
    // and the summed balance is used as it always was.
    let tariff = quota.tariff_quota.as_ref().filter(|tariff| {
        used_percent(tariff.current.as_deref(), tariff.maximum.as_deref()).is_some()
    });
    let (current, maximum) = match tariff {
        Some(tariff) => (tariff.current.as_deref(), tariff.maximum.as_deref()),
        None => (quota.current.as_deref(), quota.maximum.as_deref()),
    };

    // type Unknown/Error (or absent current/maximum) means the IDE is installed
    // and its config was read, but this account has no AI quota to report. The
    // credential is fine and nothing is broken, so this is neither an absent
    // credential nor a failure -- a consumer must not count it as something to
    // fix, or the number never reaches zero.
    let used = used_percent(current, maximum).ok_or_else(|| {
        FetchError::NoQuotaReported(format!(
            "jetbrains: no active quota (type {:?})",
            quota.kind.as_deref().unwrap_or("?")
        ))
    })?;

    // Parsed ONCE and both halves kept. The reset instant and the stated
    // mechanic come out of the same object, so reading it twice would let them
    // disagree about a payload they both describe.
    let refill = extract_option_value(xml, "nextRefill")
        .and_then(|raw| serde_json::from_str::<NextRefill>(&decode_html_entities(&raw)).ok());

    let regeneration = refill.as_ref().and_then(regeneration_from);

    // The tariff's own period, and only when the payload states it and the
    // window IS the tariff. Over the summed balance a length would claim the
    // purchased part resets too, which it never does.
    let window_minutes = tariff.and_then(|_| {
        refill
            .as_ref()?
            .tariff
            .as_ref()?
            .duration
            .as_deref()
            .and_then(iso8601_duration_minutes)
            .filter(|minutes| *minutes > 0)
    });

    // Published only beside a tariff window. Without the split the top-up is
    // already inside the summed window, and a pool too would count it twice.
    let top_up = tariff
        .and(quota.top_up_quota.as_ref())
        .and_then(top_up_pool);

    let resets_at = refill.and_then(|r| r.next).filter(|s| !s.trim().is_empty());

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
    let (used_count, total_count) = current
        .and_then(|c| c.trim().parse::<f64>().ok())
        .zip(maximum.and_then(|m| m.trim().parse::<f64>().ok()))
        .map_or((None, None), |(current, maximum)| {
            crate::model::window_counts(current, maximum)
        });

    Ok(Normalized {
        usage: Usage {
            primary: Some(RateWindow {
                used_percent: used,
                raw_used_percent: None,
                resets_at: Some(resets_at),
                // `nextRefill.tariff.duration` when the window is the tariff
                // and the payload states a period; absent otherwise. The
                // observed payload splits the balance into a tariff that
                // refills on that period and a purchased top-up that does not
                // (insula#1). The window measures the tariff alone, so the
                // period is its length; the top-up is published as a pool.
                window_minutes,
                used_count,
                total_count,
                regeneration,
            }),
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        },
        top_up,
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
            let normalized = normalize(&bytes)?;
            let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, "api", normalized.usage);
            // Absent rather than empty when there is no top-up: an empty list
            // would state that the provider reports none.
            entry.spend = normalized.top_up.map(|pool| vec![pool]);
            Ok(entry)
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
    /// A reading of `current` as "remaining" -- the plausible misreading --
    /// would report 99.19% used on an account that has spent under one percent.
    ///
    /// DELIBERATELY CHANGED from the original pin. This used to assert the
    /// percent over the summed balance (8100/1207000 = 0.6711%) with no window
    /// length. The window is now the refilling tariff alone (8100/1000000 =
    /// 0.81%), which makes the tariff's stated PT720H its length, and the
    /// purchased top-up that never refills is published as its own pool. Over
    /// the sum the window could not reach 100% while top-up remained, so it
    /// showed headroom once the part that refills was gone.
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

        let normalized = normalize(xml.as_bytes()).expect("the observed payload must normalize");
        let window = normalized.usage.primary.expect("a primary window");

        assert!(
            (window.used_percent - 0.81).abs() < 1e-9,
            "expected tariff 8100/1000000 = 0.81%, got {}",
            window.used_percent
        );
        assert_eq!(window.used_count, Some(8100.0));
        assert_eq!(
            window.total_count,
            Some(1_000_000.0),
            "the tariff's own maximum"
        );
        assert_eq!(
            window.resets_at.as_deref(),
            Some("2026-07-15T06:00:00.000Z")
        );
        // PT720H, stated by the payload and describing exactly this window.
        assert_eq!(window.window_minutes, Some(43_200));

        let pool = normalized
            .top_up
            .expect("the top-up is published as a pool");
        assert_eq!(pool.id, "topUpQuota");
        assert_eq!(pool.funding, PoolFunding::Purchased);
        assert_eq!(pool.basis, PoolBasis::Reported);
        assert_eq!(pool.resets_at, None, "the top-up does not refill");
        // "207000.000" read exactly: 207000000 thousandths.
        let quota = |minor| Amount {
            minor,
            exponent: 3,
            unit: TOP_UP_UNIT.to_string(),
        };
        assert_eq!(pool.remaining, Some(quota(207_000_000)));
        assert_eq!(pool.total, Some(quota(207_000_000)));
    }

    /// The case the split exists for: the tariff is spent, the top-up untouched.
    ///
    /// Over the summed balance this read 1000000/1207000 = 82.9% -- apparent
    /// headroom on an account whose refilling allowance is gone and which is now
    /// drawing purchased quota. The window must say 100% and the pool must show
    /// the top-up still full.
    #[test]
    fn a_spent_tariff_reads_full_while_the_top_up_stays_untouched() {
        let quota_info = r#"{"type":"Available","current":"1000000.000","maximum":"1207000.000","tariffQuota":{"current":"1000000.000","maximum":"1000000","available":"0.000"},"topUpQuota":{"current":"0","maximum":"207000.000","available":"207000.000"}}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"PT720H"}}"#;
        let normalized =
            normalize(observed_xml(quota_info, next_refill).as_bytes()).expect("must normalize");

        let window = normalized.usage.primary.expect("a primary window");
        assert_eq!(
            window.used_percent, 100.0,
            "the refilling part is exhausted"
        );
        let pool = normalized.top_up.expect("a top-up pool");
        assert_eq!(pool.remaining, pool.total, "the top-up is untouched");
        assert_eq!(pool.remaining.map(|a| a.minor), Some(207_000_000));
    }

    /// Without `tariffQuota` there is nothing to split, so the output is the
    /// summed window exactly as before: its percent and counts, no length, and
    /// no pool -- even when `topUpQuota` is present, since the summed window
    /// already contains the top-up and a pool would count it twice.
    #[test]
    fn without_a_tariff_split_the_summed_window_is_unchanged() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","topUpQuota":{"current":"0","maximum":"207000.000","available":"207000.000"}}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"PT720H"}}"#;
        let normalized =
            normalize(observed_xml(quota_info, next_refill).as_bytes()).expect("must normalize");

        let window = normalized.usage.primary.expect("a primary window");
        assert!(
            (window.used_percent - 8100.0 / 1_207_000.0 * 100.0).abs() < 1e-9,
            "expected the summed 0.6711%, got {}",
            window.used_percent
        );
        assert_eq!(window.used_count, Some(8100.0));
        assert_eq!(window.total_count, Some(1_207_000.0));
        assert_eq!(window.window_minutes, None);
        assert!(normalized.top_up.is_none(), "{:?}", normalized.top_up);
    }

    /// A tariff window whose period the payload does not state has no length.
    /// The length is never assumed from what the observed payload happened to
    /// carry.
    #[test]
    fn a_refill_without_a_stated_duration_gives_no_window_length() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","tariffQuota":{"current":"8100.000","maximum":"1000000","available":"991900.000"}}"#;
        let next_refill =
            r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000"}}"#;
        let window = normalize_usage(observed_xml(quota_info, next_refill).as_bytes())
            .expect("must normalize")
            .primary
            .expect("a primary window");

        assert!(
            (window.used_percent - 0.81).abs() < 1e-9,
            "still the tariff"
        );
        assert_eq!(window.window_minutes, None);
    }

    /// A top-up figure that is not exactly a decimal number publishes no pool,
    /// and the tariff window is unaffected by it.
    #[test]
    fn an_unreadable_top_up_publishes_no_pool_and_leaves_the_window() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","tariffQuota":{"current":"8100.000","maximum":"1000000","available":"991900.000"},"topUpQuota":{"current":"0","maximum":"207,000.000","available":"207000.000"}}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"PT720H"}}"#;
        let normalized = normalize(observed_xml(quota_info, next_refill).as_bytes())
            .expect("an unreadable pool must not fail the window");

        assert!(normalized.top_up.is_none(), "{:?}", normalized.top_up);
        let window = normalized.usage.primary.expect("a primary window");
        assert!((window.used_percent - 0.81).abs() < 1e-9);
        assert_eq!(window.total_count, Some(1_000_000.0));
        assert_eq!(window.window_minutes, Some(43_200));
    }

    /// The published entry passes the wire checker's pool rules, the
    /// remaining-within-total comparison among them.
    #[test]
    fn the_top_up_pool_satisfies_the_wire_rules() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","tariffQuota":{"current":"8100.000","maximum":"1000000","available":"991900.000"},"topUpQuota":{"current":"0","maximum":"207000.000","available":"207000.000"}}"#;
        // The checker also judges the window against the clock, so the reset is
        // placed inside the 30-day period from now and the entry is stamped.
        let now = crate::wire_sanity::now();
        let reset = (now + chrono::Duration::days(10)).to_rfc3339();
        let next_refill = format!(
            r#"{{"type":"Known","next":"{reset}","tariff":{{"amount":"1000000","duration":"PT720H"}}}}"#
        );
        let normalized =
            normalize(observed_xml(quota_info, &next_refill).as_bytes()).expect("must normalize");
        let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, "api", normalized.usage);
        entry.fetched_at = Some(now.to_rfc3339());
        entry.spend = normalized.top_up.map(|pool| vec![pool]);

        let report = crate::wire_sanity::check_entries(&[entry], now);
        assert_eq!(report.pools_checked, 1);
        assert_eq!(report.pool_amounts_checked, 2);
        assert_eq!(report.pool_comparisons, 1, "remaining was bounded by total");
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    /// The observed payload's stated mechanic, ASSERTED ON THE WIRE BYTES.
    ///
    /// Deliberately not a normalizer assertion. The defect this field exists to
    /// prevent is a value escaping into the WRONG FIELD -- a replenishment
    /// projected onto `resetsAt`, which is unrecoverable once published -- so the
    /// check has to be what a consumer actually receives.
    ///
    /// Same capture as `the_observed_credentialed_payload_normalizes_as_measured`:
    /// `tariff` states a 1,000,000-unit grant on a PT720H period, arriving whole
    /// at `next`.
    #[test]
    fn the_observed_tariff_reaches_the_wire_as_a_stated_cliff() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000","tariffQuota":{"current":"8100.000","maximum":"1000000","available":"991900.000"}}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"PT720H"}}"#;
        let usage = normalize_usage(observed_xml(quota_info, next_refill).as_bytes())
            .expect("the observed payload must normalize");

        let json = serde_json::to_value(&usage).expect("usage must serialize");
        let window = &json["primary"];

        assert_eq!(
            window["regeneration"]["mechanic"], "cliff",
            "the payload names an amount arriving at one instant, which is a cliff"
        );
        assert_eq!(window["regeneration"]["rate"]["amount"], 1_000_000.0);
        assert_eq!(
            window["regeneration"]["rate"]["perMinutes"], 43_200,
            "PT720H is 43200 minutes"
        );

        // The period also reaches `windowMinutes` now, and that is not a leak:
        // since the window became the tariff alone, the tariff's period IS its
        // length. It used to be asserted absent because the window measured the
        // tariff plus a purchased top-up that never refills. The reset stays in
        // `resetsAt`, where it belongs.
        assert_eq!(window["windowMinutes"], 43_200);
        assert_eq!(window["resetsAt"], "2026-07-15T06:00:00.000Z");
    }

    /// A payload stating only an instant emits NO mechanic.
    ///
    /// The control, and the one that keeps this consistent with the anthropic
    /// decision: an upstream giving a reset time states WHEN quota returns and
    /// nothing about HOW, and `resetsAt` already carries that. Without this case a
    /// producer stamping `cliff` on every window with a reset would pass, and it
    /// would be minting an upstream claim out of our own reading.
    #[test]
    fn a_refill_instant_alone_states_no_mechanic() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000"}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z"}"#;
        let usage = normalize_usage(observed_xml(quota_info, next_refill).as_bytes())
            .expect("must normalize");
        let window = usage.primary.expect("a primary window");

        assert!(
            window.regeneration.is_none(),
            "a bare instant is not a statement about how quota returns"
        );
        assert!(
            window.resets_at.is_some(),
            "the instant itself still publishes"
        );
    }

    /// An unreadable period keeps the mechanic and drops the rate.
    ///
    /// The shipped type documents this shape as real and common -- a mechanic
    /// described without a quantity. The alternative, dropping the whole object,
    /// would discard a fact the payload does state (the grant arrives whole at an
    /// instant) because a second fact was unreadable.
    #[test]
    fn an_unparseable_period_keeps_the_mechanic_without_a_rate() {
        let quota_info = r#"{"type":"Available","current":"8100.000","maximum":"1207000.000"}"#;
        let next_refill = r#"{"type":"Known","next":"2026-07-15T06:00:00.000Z","tariff":{"amount":"1000000","duration":"P1M"}}"#;
        let usage = normalize_usage(observed_xml(quota_info, next_refill).as_bytes())
            .expect("must normalize");
        let regeneration = usage
            .primary
            .expect("a primary window")
            .regeneration
            .expect("an amount was stated, so the mechanic stands");

        assert_eq!(regeneration.mechanic, "cliff");
        assert!(
            regeneration.rate.is_none(),
            "P1M is months, which has no fixed length in minutes"
        );
    }

    /// `M` means months before the T and minutes after it.
    ///
    /// A factor of about forty-four thousand between two strings one character
    /// apart, and only position separates them. Months are refused rather than
    /// approximated: thirty days is a guess, and this parser feeds a field whose
    /// entire purpose is carrying what the upstream STATED.
    #[test]
    fn the_duration_parser_reads_periods_and_refuses_ambiguous_ones() {
        assert_eq!(iso8601_duration_minutes("PT720H"), Some(43_200));
        assert_eq!(iso8601_duration_minutes("PT90M"), Some(90));
        assert_eq!(iso8601_duration_minutes("P30D"), Some(43_200));
        assert_eq!(iso8601_duration_minutes("P1DT2H30M"), Some(1_590));
        assert_eq!(iso8601_duration_minutes("P1W"), Some(10_080));

        // Refused, not approximated.
        assert_eq!(
            iso8601_duration_minutes("P1M"),
            None,
            "months are not fixed"
        );
        assert_eq!(iso8601_duration_minutes("P1Y"), None, "years are not fixed");
        // Malformed rather than ambiguous.
        assert_eq!(iso8601_duration_minutes("720H"), None, "no P prefix");
        assert_eq!(
            iso8601_duration_minutes("PT720"),
            None,
            "digits with no unit"
        );
        assert_eq!(iso8601_duration_minutes("PTXH"), None);
    }

    /// Build the IDE's XML around two JSON blobs, escaped as the IDE escapes them.
    fn observed_xml(quota_info: &str, next_refill: &str) -> String {
        let escape = |raw: &str| {
            raw.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
        };
        format!(
            r#"<application><component name="AIAssistantQuotaManager2"><option name="quotaInfo" value="{}" /><option name="nextRefill" value="{}" /></component></application>"#,
            escape(quota_info),
            escape(next_refill)
        )
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
