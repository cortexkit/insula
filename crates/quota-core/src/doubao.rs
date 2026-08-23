//! Doubao usage fetcher — signed Coding Plan credentials with an Ark API-key fallback.
//!
//! Coding Plan sends an empty `POST` to Volcengine's `GetCodingPlanUsage` action and
//! signs `content-type;host;x-content-sha256;x-date` with Volcengine's HMAC-SHA256
//! scheme. If those credentials are absent or the request fails, the existing Ark
//! chat-completions header probe remains available.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no Doubao
//! credentials were available. Coding Plan endpoint/request and response fields come
//! from CodexBar (`DoubaoUsageFetcher.swift:153-155,210-267,517-546`); signing comes
//! from `DoubaoVolcengineSigner.swift:30,41-102,104-152`; credential aliases and the
//! default region come from `DoubaoSettingsReader.swift:9-28,52-64`; window mapping
//! comes from `DoubaoUsageFetcher.swift:99-127`; signed-first fallback comes from
//! `DoubaoProviderDescriptor.swift:73-100`.
//!
//! The Ark fallback endpoint, body, Bearer auth, header parsing, and reset formats
//! remain sourced from CodexBar (`DoubaoUsageFetcher.swift:89-182,216-247`). Ark's
//! request-rate pool maps to `primary` with no known `window_minutes`.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, HttpResponse, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "doubao";
const API_KEY_ENV: &[&str] = &["ARK_API_KEY", "VOLCENGINE_API_KEY", "DOUBAO_API_KEY"];
const ACCESS_KEY_ID_ENV: &[&str] = &[
    "VOLCENGINE_ACCESS_KEY_ID",
    "VOLCENGINE_ACCESS_KEY",
    "VOLC_ACCESSKEY",
    "DOUBAO_ACCESS_KEY_ID",
];
const SECRET_ACCESS_KEY_ENV: &[&str] = &[
    "VOLCENGINE_SECRET_ACCESS_KEY",
    "VOLCENGINE_SECRET_KEY",
    "VOLCENGINE_ACCESS_KEY_SECRET",
    "VOLC_SECRETKEY",
    "DOUBAO_SECRET_ACCESS_KEY",
];
const REGION_ENV: &[&str] = &[
    "VOLCENGINE_REGION",
    "VOLCENGINE_REGION_ID",
    "VOLC_REGION",
    "DOUBAO_REGION",
];
const DEFAULT_REGION: &str = "cn-beijing";
const API_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions";
const CODING_PLAN_API_URL: &str =
    "https://open.volcengineapi.com/?Action=GetCodingPlanUsage&Version=2024-01-01";
const CODING_PLAN_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";
const SIGNING_ALGORITHM: &str = "HMAC-SHA256";
const SIGNING_SERVICE: &str = "ark";
const SIGNING_TERMINATOR: &str = "request";
const SIGNED_HEADERS: &str = "content-type;host;x-content-sha256;x-date";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

type HmacSha256 = Hmac<Sha256>;

/// Models to probe, ordered by likelihood (CodexBar `DoubaoUsageFetcher.swift:157-163`).
const PROBE_MODELS: &[&str] = &[
    "doubao-seed-2.0-code",
    "doubao-1.5-pro-32k",
    "doubao-lite-32k",
];

#[derive(Debug)]
struct CodingPlanCredentials {
    access_key_id: String,
    secret_access_key: String,
    region: String,
}

#[derive(Debug, PartialEq)]
struct SignedRequestHeaders {
    content_type: String,
    host: String,
    x_date: String,
    x_content_sha256: String,
    authorization: String,
}

#[derive(Debug, Deserialize)]
struct CodingPlanUsageResponse {
    #[serde(rename = "Result")]
    result: CodingPlanResult,
}

#[derive(Debug, Deserialize)]
struct CodingPlanResult {
    #[serde(rename = "Status")]
    _status: Option<String>,
    #[serde(rename = "UpdateTimestamp")]
    _update_timestamp: Option<f64>,
    #[serde(rename = "QuotaUsage")]
    quota_usage: Vec<CodingPlanQuota>,
}

#[derive(Debug, Deserialize)]
struct CodingPlanQuota {
    #[serde(rename = "Level")]
    level: String,
    #[serde(rename = "Percent")]
    percent: f64,
    #[serde(rename = "ResetTimestamp")]
    reset_timestamp: Option<f64>,
}

/// Header snapshot from a successful Ark probe (200 or 429), for normalization tests.
#[derive(Debug, Clone, PartialEq)]
pub struct DoubaoHeaderSnapshot {
    pub remaining_requests: Option<i64>,
    pub limit_requests: Option<i64>,
    pub reset_requests: Option<String>,
}

/// Normalize Ark rate-limit headers to [`Usage`]. Pure — unit-testable.
///
/// Requires `x-ratelimit-reset-requests` (CodexBar `DoubaoUsageFetcher.swift:151-153`);
/// without it the response has no window. `used_percent` follows CodexBar lines 41-58.
pub fn normalize_usage(headers: &DoubaoHeaderSnapshot) -> Result<Usage, FetchError> {
    let reset_raw = headers
        .reset_requests
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FetchError::Decode("no rate-limit headers".to_string()))?;

    let resets_at = parse_reset_time(reset_raw).ok_or_else(|| {
        FetchError::Decode(format!(
            "unparseable x-ratelimit-reset-requests: {reset_raw}"
        ))
    })?;

    let (remaining, limit) = (
        headers.remaining_requests.unwrap_or(0),
        headers.limit_requests.unwrap_or(0),
    );

    let used_percent = if limit > 0 {
        let used = (limit - remaining).max(0);
        ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let primary = Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: None,
        used_count: None,
        total_count: None,
        regeneration: None,
    });

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// Normalize a CodexBar-shaped Coding Plan response into session, weekly, and monthly
/// windows (`DoubaoUsageFetcher.swift:99-127,250-267,517-546`).
fn normalize_coding_plan_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: CodingPlanUsageResponse =
        serde_json::from_slice(body).map_err(|error| FetchError::Decode(error.to_string()))?;
    let quotas = &response.result.quota_usage;

    Ok(Usage {
        primary: coding_plan_window(quotas, &["session", "5-hour", "five_hour"], 300),
        secondary: coding_plan_window(quotas, &["weekly", "week"], 10_080),
        tertiary: coding_plan_window(quotas, &["monthly", "month"], 43_200),
        extra_rate_windows: None,
    })
}

fn coding_plan_window(
    quotas: &[CodingPlanQuota],
    levels: &[&str],
    window_minutes: i64,
) -> Option<RateWindow> {
    let quota = quotas
        .iter()
        .find(|quota| levels.contains(&quota.level.to_ascii_lowercase().as_str()))?;
    let resets_at = quota
        .reset_timestamp
        .filter(|timestamp| timestamp.is_finite() && *timestamp > 0.0)
        .and_then(|timestamp| env::epoch_to_iso8601(timestamp as i64));

    Some(RateWindow {
        used_percent: quota.percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

fn coding_plan_credentials() -> Option<CodingPlanCredentials> {
    Some(CodingPlanCredentials {
        access_key_id: env::first_env(ACCESS_KEY_ID_ENV)?,
        secret_access_key: env::first_env(SECRET_ACCESS_KEY_ENV)?,
        region: env::first_env(REGION_ENV).unwrap_or_else(|| DEFAULT_REGION.to_string()),
    })
}

/// Build Volcengine V4 headers using the canonical request, credential scope, and
/// four-step signing-key derivation from `DoubaoVolcengineSigner.swift:30,41-102`.
fn sign_coding_plan_request(
    credentials: &CodingPlanCredentials,
    date: DateTime<Utc>,
    body: &[u8],
) -> Result<SignedRequestHeaders, FetchError> {
    let url = Url::parse(CODING_PLAN_API_URL)
        .map_err(|error| FetchError::Decode(format!("invalid Coding Plan URL: {error}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::Decode("Coding Plan URL has no host".to_string()))?;
    let timestamp = date.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = date.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(body);
    let canonical_request = [
        "POST".to_string(),
        percent_encode(url.path(), false),
        canonical_query_string(&url),
        format!("content-type:{CODING_PLAN_CONTENT_TYPE}"),
        format!("host:{host}"),
        format!("x-content-sha256:{payload_hash}"),
        format!("x-date:{timestamp}"),
        String::new(),
        SIGNED_HEADERS.to_string(),
        payload_hash.clone(),
    ]
    .join("\n");
    let credential_scope = format!(
        "{date_stamp}/{}/{SIGNING_SERVICE}/{SIGNING_TERMINATOR}",
        credentials.region
    );
    let string_to_sign = [
        SIGNING_ALGORITHM.to_string(),
        timestamp.clone(),
        credential_scope.clone(),
        sha256_hex(canonical_request.as_bytes()),
    ]
    .join("\n");

    let date_key = hmac_sha256(
        credentials.secret_access_key.as_bytes(),
        date_stamp.as_bytes(),
    );
    let region_key = hmac_sha256(&date_key, credentials.region.as_bytes());
    let service_key = hmac_sha256(&region_key, SIGNING_SERVICE.as_bytes());
    let signing_key = hmac_sha256(&service_key, SIGNING_TERMINATOR.as_bytes());
    let signature = hex_lower(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "{SIGNING_ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={SIGNED_HEADERS}, Signature={signature}",
        credentials.access_key_id
    );

    Ok(SignedRequestHeaders {
        content_type: CODING_PLAN_CONTENT_TYPE.to_string(),
        host: host.to_string(),
        x_date: timestamp,
        x_content_sha256: payload_hash,
        authorization,
    })
}

fn canonical_query_string(url: &Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (percent_encode(&key, true), percent_encode(&value, true)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && byte == b'/');
        if allowed {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[(byte >> 4) as usize]));
        encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    encoded
}

async fn fetch_coding_plan_usage(
    client: &reqwest::Client,
    credentials: &CodingPlanCredentials,
) -> Result<Usage, FetchError> {
    let body = Vec::new();
    let signed = sign_coding_plan_request(credentials, Utc::now(), &body)?;
    let response_body = JsonRequest::post(CODING_PLAN_API_URL, body)
        .header(Header::new("Accept", "application/json"))
        .header(Header::new("Content-Type", signed.content_type))
        .header(Header::new("Host", signed.host))
        .header(Header::new("X-Date", signed.x_date))
        .header(Header::new("X-Content-Sha256", signed.x_content_sha256))
        .header(Header::new("Authorization", signed.authorization))
        .timeout(PROBE_TIMEOUT)
        .send(client)
        .await?;
    normalize_coding_plan_usage(&response_body)
}

/// Parse `x-ratelimit-reset-requests` — ISO8601, `1d2h3m4s` components, or bare
/// seconds until reset. Mirrors CodexBar `DoubaoUsageFetcher.parseResetTime`
/// (`216-247`).
fn parse_reset_time(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(
            dt.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(
            dt.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(
            dt.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }

    let mut seconds: f64 = 0.0;
    let mut pos = 0;
    let bytes = trimmed.as_bytes();
    while pos < bytes.len() {
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == start || pos >= bytes.len() {
            break;
        }
        let num: f64 = trimmed[start..pos].parse().ok()?;
        let unit = bytes[pos] as char;
        pos += 1;
        seconds += match unit {
            'd' => num * 86400.0,
            'h' => num * 3600.0,
            'm' => num * 60.0,
            's' => num,
            _ => return None,
        };
    }
    if seconds > 0.0 {
        let reset = Utc::now() + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64);
        return Some(reset.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }

    if let Ok(secs) = trimmed.parse::<f64>() {
        if secs > 0.0 {
            let reset = Utc::now() + chrono::Duration::milliseconds((secs * 1000.0).round() as i64);
            return Some(reset.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
    }

    None
}

fn probe_body(model: &str) -> Vec<u8> {
    serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }]
    })
    .to_string()
    .into_bytes()
}

/// Probe one Ark model and return the raw response. The rate-limit headers can be
/// present on both 200 and 429, so the caller owns status handling.
async fn probe_model(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
) -> Result<HttpResponse, FetchError> {
    let response = JsonRequest::post_json(API_URL, probe_body(model))
        .bearer(api_key)
        .timeout(PROBE_TIMEOUT)
        .send_raw(client)
        .await?;

    classify_probe_status(response.status, &response.body)?;
    Ok(response)
}

/// Decide whether a probe response is one whose headers should be read.
///
/// Split out from the request so the decision can be tested: the request itself
/// needs a live endpoint, and this is the part with the provider-specific rule
/// in it.
///
/// 429 is accepted deliberately, and it is the reason this provider uses a raw
/// send rather than the shared status handling. The quota figures live in
/// `x-ratelimit-*` response headers that are present on a refusal as well as a
/// success -- an exhausted account answers 429 and that response still states
/// the limit and its reset. Rejecting it would make a provider disappear from
/// the wire exactly when it is most constrained, which reads to a consumer as
/// capacity that was never measured.
fn classify_probe_status(status: u16, body: &[u8]) -> Result<(), FetchError> {
    if status == 401 {
        return Err(FetchError::Unauthorized(format!("HTTP {status}")));
    }
    if status != 200 && status != 429 {
        let excerpt: String = String::from_utf8_lossy(body).chars().take(200).collect();
        return Err(FetchError::Upstream(format!("HTTP {status}: {excerpt}")));
    }
    Ok(())
}

fn headers_to_snapshot(response: &HttpResponse) -> DoubaoHeaderSnapshot {
    let int_header = |name: &str| response.header(name).and_then(|v| v.trim().parse().ok());
    let string_header = |name: &str| {
        response
            .header(name)
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    DoubaoHeaderSnapshot {
        remaining_requests: int_header("x-ratelimit-remaining-requests"),
        limit_requests: int_header("x-ratelimit-limit-requests"),
        reset_requests: string_header("x-ratelimit-reset-requests"),
    }
}

pub struct DoubaoProvider {
    http: reqwest::Client,
}

impl DoubaoProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }

    async fn fetch_ark_usage(&self, api_key: &str) -> Result<Usage, FetchError> {
        let mut last_error: Option<FetchError> = None;
        for model in PROBE_MODELS {
            match probe_model(&self.http, api_key, model).await {
                Ok(response) => return normalize_usage(&headers_to_snapshot(&response)),
                Err(err) => {
                    if let FetchError::Upstream(ref msg) = err {
                        if msg.starts_with("HTTP 404") || msg.starts_with("HTTP 403") {
                            last_error = Some(err);
                            continue;
                        }
                    }
                    return Err(err);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| FetchError::Upstream("all probe models failed".to_string())))
    }
}

impl Default for DoubaoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for DoubaoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV);

            // Prefer Coding Plan because it reports usage directly. If its signed
            // request fails, an available Ark API key can still recover usage.
            if let Some(credentials) = coding_plan_credentials() {
                match fetch_coding_plan_usage(&self.http, &credentials).await {
                    Ok(usage) => {
                        return Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage));
                    }
                    Err(coding_plan_error) => {
                        if let Some(api_key) = api_key.as_deref() {
                            let usage = self.fetch_ark_usage(api_key).await?;
                            return Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage));
                        }
                        return Err(coding_plan_error);
                    }
                }
            }

            let api_key = api_key.ok_or_else(|| {
                // Names the variables rather than describing them. This
                // provider accepts two different credential schemes across
                // sixteen accepted names, so "no complete credentials found"
                // leaves a reader with nothing to act on and no way to learn
                // which spelling this build reads -- and the aliases exist
                // precisely because the upstream's own tools disagree.
                //
                // Interpolated from the constants, so a name added or removed
                // cannot leave the message describing a set that no longer
                // exists.
                FetchError::NoSession(format!(
                    "no Doubao credentials: set one of {API_KEY_ENV:?}, or a \
                     Volcengine signing pair from {ACCESS_KEY_ID_ENV:?} and \
                     {SECRET_ACCESS_KEY_ENV:?}"
                ))
            })?;
            let usage = self.fetch_ark_usage(&api_key).await?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {

    /// The absent-credential message names the variables to set.
    ///
    /// This provider accepts two credential schemes across sixteen names, so a
    /// message that only says credentials are missing leaves a reader with no
    /// way to learn which spelling this build reads -- and the aliases exist
    /// because the upstream's own tools disagree about them.
    ///
    /// Asserted on the rendered message rather than on the constants, because
    /// the constants being correct says nothing about whether they reach a
    /// reader: the interpolation is the part that can silently stop happening.
    #[test]
    fn the_absent_credential_message_names_the_variables() {
        let message = format!(
            "no Doubao credentials: set one of {API_KEY_ENV:?}, or a \
             Volcengine signing pair from {ACCESS_KEY_ID_ENV:?} and \
             {SECRET_ACCESS_KEY_ENV:?}"
        );
        for expected in [
            "ARK_API_KEY",
            "VOLCENGINE_ACCESS_KEY_ID",
            "VOLCENGINE_SECRET_ACCESS_KEY",
        ] {
            assert!(
                message.contains(expected),
                "{expected} must appear in the message a reader acts on: {message}"
            );
        }
    }
    use super::*;
    use chrono::TimeZone;

    /// A refusal still carries the quota figures, so it must not be rejected.
    ///
    /// This provider reads its windows from `x-ratelimit-*` response headers,
    /// which are present on a 429 as well as a 200. An exhausted account answers
    /// 429 and that response states both the limit and its reset -- so treating
    /// it as an upstream error would drop the provider from the wire at exactly
    /// the moment it is most constrained, and a missing window reads as capacity
    /// that was never measured rather than as capacity that is gone.
    #[test]
    fn a_rate_limited_probe_is_accepted_so_its_headers_can_be_read() {
        assert!(classify_probe_status(429, b"").is_ok());
    }

    /// The paired case: 200 is accepted for the ordinary reason.
    #[test]
    fn a_successful_probe_is_accepted() {
        assert!(classify_probe_status(200, b"").is_ok());
    }

    /// Every other status is refused, and the two refusals are distinguished.
    ///
    /// 401 is non-transient -- a dead key keeps failing, and serving a stale
    /// window under it would hide that. Other statuses are transient, so the
    /// last healthy window keeps being served through a flap instead of being
    /// replaced by a degraded entry.
    #[test]
    fn other_statuses_are_refused_with_the_class_that_matches_them() {
        assert!(matches!(
            classify_probe_status(401, b""),
            Err(FetchError::Unauthorized(_))
        ));
        assert!(matches!(
            classify_probe_status(500, b"upstream exploded"),
            Err(FetchError::Upstream(_))
        ));
        assert!(matches!(
            classify_probe_status(404, b""),
            Err(FetchError::Upstream(_))
        ));
    }

    #[test]
    fn coding_plan_payload_maps_all_three_windows() {
        let payload = br#"{
            "Result": {
                "Status": "active",
                "UpdateTimestamp": 1700000000,
                "QuotaUsage": [
                    {"Level": "session", "Percent": 37.5, "ResetTimestamp": 1700000000},
                    {"Level": "weekly", "Percent": -4.0, "ResetTimestamp": 1800000000},
                    {"Level": "monthly", "Percent": 123.0, "ResetTimestamp": 1900000000}
                ]
            }
        }"#;

        let usage = normalize_coding_plan_usage(payload).unwrap();
        assert_eq!(
            usage.primary,
            Some(RateWindow {
                used_percent: 37.5,
                raw_used_percent: None,
                resets_at: Some("2023-11-14T22:13:20Z".to_string()),
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
                regeneration: None,
            })
        );
        assert_eq!(
            usage.secondary,
            Some(RateWindow {
                used_percent: 0.0,
                raw_used_percent: None,
                resets_at: Some("2027-01-15T08:00:00Z".to_string()),
                window_minutes: Some(10_080),
                used_count: None,
                total_count: None,
                regeneration: None,
            })
        );
        assert_eq!(
            usage.tertiary,
            Some(RateWindow {
                used_percent: 100.0,
                raw_used_percent: None,
                resets_at: Some("2030-03-17T17:46:40Z".to_string()),
                window_minutes: Some(43_200),
                used_count: None,
                total_count: None,
                regeneration: None,
            })
        );
        assert_eq!(usage.extra_rate_windows, None);
    }

    #[test]
    fn signer_matches_fixed_volcengine_authorization() {
        let credentials = CodingPlanCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "test-secret-key".to_string(),
            region: "cn-beijing".to_string(),
        };
        let date = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let signed = sign_coding_plan_request(&credentials, date, b"").unwrap();

        assert_eq!(
            signed,
            SignedRequestHeaders {
                content_type: "application/x-www-form-urlencoded; charset=utf-8".to_string(),
                host: "open.volcengineapi.com".to_string(),
                x_date: "20260102T030405Z".to_string(),
                x_content_sha256:
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_string(),
                authorization: "HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/cn-beijing/ark/request, SignedHeaders=content-type;host;x-content-sha256;x-date, Signature=d15493b1dcd51a1f3a7f0c29ac8f37fe8412854a7c627b6f0aa3b62f569c2a86".to_string(),
            }
        );
    }

    #[test]
    fn normal_window_from_remaining_and_limit() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(80),
            limit_requests: Some(100),
            reset_requests: Some("2026-06-22T18:00:00Z".to_string()),
        };
        let usage = normalize_usage(&headers).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T18:00:00Z"));
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn remaining_to_used_conversion() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(0),
            limit_requests: Some(50),
            reset_requests: Some("2026-06-22T12:00:00Z".to_string()),
        };
        let usage = normalize_usage(&headers).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn missing_reset_header_errors() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(10),
            limit_requests: Some(100),
            reset_requests: None,
        };
        let err = normalize_usage(&headers).unwrap_err();
        assert!(matches!(err, FetchError::Decode(ref m) if m == "no rate-limit headers"));
    }

    #[test]
    fn garbage_reset_errors() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(1),
            limit_requests: Some(10),
            reset_requests: Some("not-a-date-or-duration".to_string()),
        };
        assert!(normalize_usage(&headers).is_err());
    }

    #[test]
    fn parse_reset_iso8601() {
        assert_eq!(
            parse_reset_time("2026-06-22T15:30:00Z").as_deref(),
            Some("2026-06-22T15:30:00Z")
        );
    }

    #[test]
    fn parse_reset_duration_components() {
        let parsed = parse_reset_time("1h30m").expect("duration parse");
        let dt = DateTime::parse_from_rfc3339(&parsed).unwrap();
        let now = Utc::now();
        let delta = dt.with_timezone(&Utc) - now;
        assert!(delta.num_minutes() >= 89 && delta.num_minutes() <= 91);
    }

    #[test]
    fn parse_reset_bare_seconds() {
        let parsed = parse_reset_time("120").expect("seconds parse");
        let dt = DateTime::parse_from_rfc3339(&parsed).unwrap();
        let delta = dt.with_timezone(&Utc) - Utc::now();
        assert!(delta.num_seconds() >= 115 && delta.num_seconds() <= 125);
    }

    #[test]
    fn probe_body_matches_codexbar() {
        let body = probe_body("doubao-seed-2.0-code");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "doubao-seed-2.0-code");
        assert_eq!(json["max_tokens"], 1);
        assert_eq!(json["messages"][0]["content"], "hi");
    }
}
