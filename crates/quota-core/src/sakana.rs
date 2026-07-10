//! Sakana AI usage — environment cookie + billing-page HTML scrape.
//!
//! Reads a complete `Cookie` header from `SAKANA_COOKIE`, then fetches the
//! subscription quota cards rendered by the Sakana AI console.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! Sakana session is available on the build machine. Credential normalization,
//! endpoint, request headers, response checks, HTML fields, UTC reset parsing,
//! and window mapping follow CodexBar
//! `Sources/CodexBarCore/Providers/Sakana/SakanaSettingsReader.swift:4-21` and
//! `Sources/CodexBarCore/Providers/Sakana/SakanaUsageFetcher.swift:40-54,135-165,182-201,349-440`.
//! Unit-tested with the billing-page fixture from
//! `Tests/CodexBarTests/SakanaUsageFetcherTests.swift:376-390`.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{NaiveDateTime, TimeZone, Utc};
use reqwest::Url;

use crate::{
    env,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "sakana";
const COOKIE_ENV: &[&str] = &["SAKANA_COOKIE"];
const BILLING_URL: &str = "https://console.sakana.ai/billing";
const BILLING_HOST: &str = "console.sakana.ai";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const FIVE_HOUR_MINUTES: i64 = 5 * 60;
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

#[derive(Debug)]
struct Paragraph {
    start: usize,
    end: usize,
    text: String,
}

/// Normalize the environment setting and the optional literal `Cookie:` prefix.
fn normalize_cookie_header(raw: &str) -> Option<String> {
    let mut value = raw.trim().to_string();
    if value.is_empty() {
        return None;
    }

    if is_wrapped_in_matching_quotes(&value) {
        value = value[1..value.len() - 1].trim().to_string();
    }

    if value
        .get(.."cookie:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
    {
        value = value["cookie:".len()..].trim().to_string();
    }

    if is_wrapped_in_matching_quotes(&value) {
        value = value[1..value.len() - 1].trim().to_string();
    }

    (!value.is_empty()).then_some(value)
}

fn is_wrapped_in_matching_quotes(value: &str) -> bool {
    value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
}

fn load_cookie_header() -> Result<String, FetchError> {
    env::first_env(COOKIE_ENV)
        .and_then(|raw| normalize_cookie_header(&raw))
        .ok_or_else(|| FetchError::NoSession("SAKANA_COOKIE is not set or empty".to_string()))
}

fn visible_text(fragment: &str) -> String {
    let mut text = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for ch in fragment.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn paragraphs(html: &str) -> Vec<Paragraph> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;

    while let Some(relative_start) = lower[cursor..].find("<p") {
        let start = cursor + relative_start;
        let after_name = lower.as_bytes().get(start + 2).copied();
        if !matches!(after_name, Some(b'>') | Some(b' ' | b'\t' | b'\r' | b'\n')) {
            cursor = start + 2;
            continue;
        }

        let Some(relative_open_end) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + relative_open_end;
        let content_start = open_end + 1;
        let Some(relative_close) = lower[content_start..].find("</p>") else {
            break;
        };
        let close_start = content_start + relative_close;
        let end = close_start + "</p>".len();
        out.push(Paragraph {
            start,
            end,
            text: visible_text(&html[content_start..close_start]),
        });
        cursor = end;
    }

    out
}

fn next_card_boundary(html: &str, after: usize) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut cursor = after;

    while let Some(relative_start) = lower[cursor..].find("<div") {
        let start = cursor + relative_start;
        let after_name = lower.as_bytes().get(start + 4).copied();
        if !matches!(after_name, Some(b'>') | Some(b' ' | b'\t' | b'\r' | b'\n')) {
            cursor = start + 4;
            continue;
        }
        let relative_end = lower[start..].find('>')?;
        let end = start + relative_end + 1;
        let opening_tag = &lower[start..end];
        if opening_tag.contains("data-slot=\"card\"")
            || opening_tag.contains("data-slot='card'")
            || opening_tag.contains("data-slot=\"card-title\"")
            || opening_tag.contains("data-slot='card-title'")
        {
            return Some(start);
        }
        cursor = end;
    }

    None
}

fn window_body<'a>(label: &str, html: &'a str) -> Option<&'a str> {
    let paragraphs = paragraphs(html);
    let label_paragraph = paragraphs
        .iter()
        .find(|paragraph| paragraph.text.eq_ignore_ascii_case(label))?;
    let body_start = label_paragraph.end;

    let next_label = paragraphs
        .iter()
        .filter(|paragraph| paragraph.start >= body_start)
        .find(|paragraph| {
            paragraph.text.eq_ignore_ascii_case("5-hour")
                || paragraph.text.eq_ignore_ascii_case("Weekly")
        })
        .map(|paragraph| paragraph.start);
    let body_end = next_label
        .into_iter()
        .chain(next_card_boundary(html, body_start))
        .min()
        .unwrap_or(html.len());
    let body = html[body_start..body_end].trim();
    (!body.is_empty()).then_some(body)
}

fn used_percent(text: &str) -> Option<f64> {
    let lower = text.to_ascii_lowercase();
    let raw = lower.strip_suffix("% used")?.trim();
    if raw.is_empty()
        || raw.chars().filter(|ch| *ch == '.').count() > 1
        || !raw.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    {
        return None;
    }
    let percent = raw.parse::<f64>().ok()?;
    (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent)
}

fn reset_value(text: &str) -> Option<&str> {
    const PREFIX: &str = "resets on ";
    text.get(..PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))?;
    let value = text[PREFIX.len()..].trim();
    (!value.is_empty()).then_some(value)
}

fn parse_reset_date(value: &str) -> Option<String> {
    let reset = NaiveDateTime::parse_from_str(value.trim(), "%B %e, %Y at %l:%M %p").ok()?;
    Some(
        Utc.from_utc_datetime(&reset)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
}

fn parse_window(
    label: &str,
    html: &str,
    window_minutes: i64,
) -> Result<Option<RateWindow>, FetchError> {
    let Some(body) = window_body(label, html) else {
        return Ok(None);
    };
    let paragraphs = paragraphs(body);
    let percent = paragraphs
        .iter()
        .find_map(|paragraph| used_percent(&paragraph.text))
        .ok_or_else(|| FetchError::Decode(format!("sakana: invalid {label} usage percentage")))?;
    let resets_at = paragraphs
        .iter()
        .find_map(|paragraph| reset_value(&paragraph.text))
        .and_then(parse_reset_date);

    Ok(Some(RateWindow {
        used_percent: percent,
        resets_at,
        window_minutes: Some(window_minutes),
    }))
}

/// Parse Sakana's server-rendered subscription quota cards.
pub fn parse_billing_html(html: &str) -> Result<Usage, FetchError> {
    let primary = parse_window("5-hour", html, FIVE_HOUR_MINUTES)?;
    let secondary = parse_window("Weekly", html, WEEKLY_MINUTES)?;
    if primary.is_none() && secondary.is_none() {
        return Err(FetchError::Decode(
            "sakana: usage limit windows were not found".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn normalize_response(status: u16, final_url: &Url, body: &[u8]) -> Result<Usage, FetchError> {
    if status == 401 || status == 403 || (300..400).contains(&status) {
        return Err(FetchError::Unauthorized(
            "sakana login is required".to_string(),
        ));
    }
    if final_url.scheme() != "https"
        || final_url
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case(BILLING_HOST))
    {
        return Err(FetchError::Unauthorized(
            "sakana billing request left console.sakana.ai".to_string(),
        ));
    }
    if status != 200 {
        return Err(FetchError::Upstream(format!(
            "sakana billing fetch failed (HTTP {status})"
        )));
    }

    let html = std::str::from_utf8(body)
        .map_err(|e| FetchError::Decode(format!("sakana billing page is not UTF-8: {e}")))?;
    if html.is_empty() {
        return Err(FetchError::Decode(
            "sakana billing page response was empty".to_string(),
        ));
    }
    parse_billing_html(html)
}

fn same_origin(original: &Url, redirected: &Url) -> bool {
    original.scheme().eq_ignore_ascii_case("https")
        && redirected.scheme().eq_ignore_ascii_case("https")
        && original.host_str().map(str::to_ascii_lowercase)
            == redirected.host_str().map(str::to_ascii_lowercase)
        && original.port_or_known_default() == redirected.port_or_known_default()
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt
            .previous()
            .first()
            .is_some_and(|original| same_origin(original, attempt.url()))
        {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// The Sakana AI usage provider.
pub struct SakanaProvider {
    http: Result<reqwest::Client, String>,
}

impl SakanaProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(redirect_policy())
                .build()
                .map_err(|error| error.to_string()),
        }
    }
}

impl Default for SakanaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for SakanaProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let cookie = load_cookie_header()?;
        let http = self.http.as_ref().map_err(|message| {
            FetchError::Upstream(format!("building Sakana HTTP client: {message}"))
        })?;
        let response = http
            .get(BILLING_URL)
            .timeout(REQUEST_TIMEOUT)
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", cookie)
            .send()
            .await
            .map_err(|error| FetchError::Upstream(error.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let body = response
            .bytes()
            .await
            .map_err(|error| FetchError::Upstream(format!("reading Sakana response: {error}")))?;
        let usage = normalize_response(status, &final_url, &body)?;

        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "web", usage))
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const BILLING_FIXTURE: &str = r#"
    <main>
      <div data-slot="card-title"><span>Standard</span><span>$20/mo</span></div>
      <div data-slot="card-title">Usage limit</div>
      <p class="font-medium text-sm">5-hour</p>
      <p class="text-muted-foreground text-xs tabular-nums">Resets on June 23, 2026 at 2:53 PM</p>
      <button aria-label="The 5-hour window starts with your first request."></button>
      <p class="text-muted-foreground text-sm">92% used</p>
      <p class="font-medium text-sm">Weekly</p>
      <p class="text-muted-foreground text-xs tabular-nums">Resets on June 29, 2026 at 12:00 AM</p>
      <button aria-label="Weekly usage resets every Monday at 00:00 UTC."></button>
      <p class="text-muted-foreground text-sm">32% used</p>
    </main>
    "#;

    #[test]
    fn billing_html_maps_five_hour_and_weekly_windows() {
        let usage = parse_billing_html(BILLING_FIXTURE).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 92.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-23T14:53:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 32.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-29T00:00:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));
        assert!(usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn cross_origin_login_redirect_is_unauthorized() {
        let login_url = Url::parse("https://auth.sakana.ai/login").unwrap();
        let result = normalize_response(200, &login_url, BILLING_FIXTURE.as_bytes());
        assert!(matches!(result, Err(FetchError::Unauthorized(_))));
    }

    #[test]
    fn missing_cookie_environment_is_no_session() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous: Option<OsString> = std::env::var_os("SAKANA_COOKIE");
        std::env::remove_var("SAKANA_COOKIE");
        let result = load_cookie_header();
        if let Some(value) = previous {
            std::env::set_var("SAKANA_COOKIE", value);
        }

        assert!(matches!(result, Err(FetchError::NoSession(_))));
    }

    #[test]
    fn cookie_setting_accepts_quotes_and_header_prefix() {
        assert_eq!(
            normalize_cookie_header("  \"Cookie: session=abc; theme=dark\"  ").as_deref(),
            Some("session=abc; theme=dark")
        );
    }
}
