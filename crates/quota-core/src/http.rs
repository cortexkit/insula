//! Shared HTTP helper for usage fetchers.
//!
//! Almost every window-bearing provider is "attach a credential header → GET (or
//! POST) one URL → decode JSON", with identical error mapping: 401/403 →
//! `Unauthorized`, any other non-2xx → `Upstream`, transport failure → `Upstream`.
//! Centralizing that here keeps each provider fetcher to its real work — auth
//! source + endpoint + window normalization — and keeps the error→FetchError
//! mapping uniform so silent-degrade behaves identically across providers.
//!
//! It deliberately returns the raw response BYTES rather than a decoded type: the
//! per-provider normalizer owns decoding (its own response shape), and keeping the
//! bytes makes those normalizers unit-testable against recorded real payloads
//! without going through this module.

use std::time::Duration;

use crate::provider::FetchError;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Percent-encode form pairs into an `application/x-www-form-urlencoded` body.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A header to attach to the request.
pub struct Header {
    pub name: &'static str,
    pub value: String,
}

/// A 2xx response exposing its headers alongside the body, for the few providers
/// whose window signal lives in RESPONSE HEADERS (e.g. `x-ratelimit-reset-*`)
/// rather than the JSON body. Most providers use [`JsonRequest::send`] (body only).
pub struct HttpResponse {
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive lookup of a response header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Header {
    pub fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(token: &str) -> Self {
        Self::new("Authorization", format!("Bearer {token}"))
    }
}

/// A small request spec the helper executes and maps to [`FetchError`].
pub struct JsonRequest {
    method: Method,
    url: String,
    headers: Vec<Header>,
    timeout: Duration,
}

enum Method {
    Get,
    Post(Vec<u8>),
}

impl JsonRequest {
    /// A GET that accepts JSON.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: vec![Header::new("Accept", "application/json")],
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// A POST with a JSON body.
    pub fn post_json(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::Post(body),
            url: url.into(),
            headers: vec![
                Header::new("Accept", "application/json"),
                Header::new("Content-Type", "application/json"),
            ],
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// A POST with an `application/x-www-form-urlencoded` body (e.g. an OAuth2
    /// token refresh). `pairs` are percent-encoded into `k=v&k=v` form.
    pub fn post_form(url: impl Into<String>, pairs: &[(&str, &str)]) -> Self {
        let body = encode_form(pairs).into_bytes();
        Self {
            method: Method::Post(body),
            url: url.into(),
            headers: vec![
                Header::new("Accept", "application/json"),
                Header::new("Content-Type", "application/x-www-form-urlencoded"),
            ],
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Attach `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> Self {
        self.header(Header::bearer(token))
    }

    /// Attach an arbitrary header.
    pub fn header(mut self, header: Header) -> Self {
        self.headers.push(header);
        self
    }

    /// Override the default 30s timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Execute the request and return the response body bytes on 2xx.
    ///
    /// Error mapping (uniform silent-degrade contract):
    /// - 401/403 → [`FetchError::Unauthorized`]
    /// - other non-2xx → [`FetchError::Upstream`] (with a short body excerpt)
    /// - transport/timeout → [`FetchError::Upstream`]
    pub async fn send(self, client: &reqwest::Client) -> Result<Vec<u8>, FetchError> {
        Ok(self.send_full(client).await?.body)
    }

    /// Like [`send`](Self::send) but also returns the response headers, for
    /// providers whose window lives in a header (e.g. `x-ratelimit-reset-*`).
    /// Same uniform error mapping (401/403 → Unauthorized, other non-2xx →
    /// Upstream, transport/timeout → Upstream).
    pub async fn send_full(self, client: &reqwest::Client) -> Result<HttpResponse, FetchError> {
        let mut builder = match &self.method {
            Method::Get => client.get(&self.url),
            Method::Post(body) => client.post(&self.url).body(body.clone()),
        }
        .timeout(self.timeout);
        for header in &self.headers {
            builder = builder.header(header.name, &header.value);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| FetchError::Upstream(e.to_string()))?;
        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| FetchError::Upstream(format!("reading body: {e}")))?;

        if status == 401 || status == 403 {
            return Err(FetchError::Unauthorized(format!("HTTP {status}")));
        }
        if !status.is_success() {
            let excerpt: String = String::from_utf8_lossy(&body).chars().take(200).collect();
            return Err(FetchError::Upstream(format!("HTTP {status}: {excerpt}")));
        }
        Ok(HttpResponse {
            headers,
            body: body.to_vec(),
        })
    }
}
