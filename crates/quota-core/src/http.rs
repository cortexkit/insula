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

/// Safety bound, not a limit tuned to observed provider payloads. A 32 MiB quota
/// response is already implausibly large, so this leaves generous compatibility
/// headroom while keeping an untrusted response from growing memory without bound.
/// Crossing the bound is a provider contract violation and therefore `Decode`:
/// retrying it as a transient upstream failure would stale-serve indefinitely.
const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Preserve far more than the 200 characters used in diagnostics while bounding
/// rejected-body draining. Normal short errors are still drained to EOF so their
/// connections remain reusable; only oversized errors forfeit pool reuse.
const ERROR_BODY_PREFIX_BYTES: usize = 8 * 1024;

/// Render a transport failure for the wire, without the request URL.
///
/// `reqwest::Error`'s `Display` appends ` for url (<the request URL>)`, so any
/// credential in that URL reaches the `error` string of a degraded entry, which
/// is published to consumers and stored by them. A query parameter is the
/// reachable case and prints verbatim; userinfo is not, because reqwest removes
/// it from the URL it retains.
///
/// The URL adds nothing a consumer can act on -- the entry already names the
/// provider -- and reqwest documents `without_url` for exactly this situation.
/// Stripping it here, at the one place transport errors become text, keeps the
/// property from depending on every provider keeping credentials out of its
/// URLs.
pub(crate) fn transport_error(error: reqwest::Error) -> FetchError {
    FetchError::Upstream(error.without_url().to_string())
}

fn body_too_large() -> FetchError {
    FetchError::Decode(format!(
        "HTTP response body exceeds {MAX_RESPONSE_BODY_BYTES}-byte safety limit"
    ))
}

async fn read_success_body(mut response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(body_too_large());
    }

    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(MAX_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
            return Err(body_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_error_body_prefix(mut response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(ERROR_BODY_PREFIX_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while body.len() < ERROR_BODY_PREFIX_BYTES {
        let Some(chunk) = response.chunk().await.map_err(transport_error)? else {
            break;
        };
        let remaining = ERROR_BODY_PREFIX_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            break;
        }
    }
    Ok(body)
}

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

/// A response exposing its status + headers alongside the body, for the few
/// providers whose window signal lives in RESPONSE HEADERS (e.g. `x-ratelimit-reset-*`)
/// rather than the JSON body. Most providers use [`JsonRequest::send`] (body only).
pub struct HttpResponse {
    pub status: u16,
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

    /// The body, refusing a success response that carries nothing.
    ///
    /// Call this instead of reading [`Self::body`] directly wherever the body is
    /// about to be parsed. An empty body reaches a normalizer as unparseable and
    /// becomes [`FetchError::Decode`], which is non-transient -- so a flapping
    /// edge that answers `200` with nothing would replace a healthy cached
    /// window with a degraded entry, and the provider reads as dead rather than
    /// briefly unreachable. Reported as [`FetchError::Upstream`] instead, which
    /// keeps serving the last good window.
    ///
    /// [`JsonRequest::send`] applies this rule already. It cannot be applied
    /// inside [`JsonRequest::send_raw`], because that exists for callers whose
    /// status and body policy is their own -- one of them reads rate-limit
    /// headers off a `429` and never looks at the body, and would break if an
    /// empty one were refused. Hence a helper the body-reading callers opt into.
    pub fn body_for_parsing(&self) -> Result<&[u8], FetchError> {
        if (200..300).contains(&self.status) && self.body.is_empty() {
            return Err(FetchError::Upstream(format!(
                "HTTP {}: {EMPTY_BODY_MARKER}",
                self.status
            )));
        }
        Ok(&self.body)
    }
}

/// The wording [`Response::body_for_parsing`] uses for an empty success body.
///
/// Shared so a provider that must tell this case apart from other transport
/// failures can match on it rather than restating the phrase. One upstream
/// answers a dead credential with an empty 200 and is otherwise unable to
/// distinguish that from a flap, so it re-attributes this specific error when
/// the credential is independently known to be expired.
pub const EMPTY_BODY_MARKER: &str = "empty response body";

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

/// How long an idle connection may stay in the pool.
///
/// Must be BELOW `refresh::BASE_INTERVAL`, so a connection is never handed back
/// out across ticks. See [`provider_client`]. Fenced by test, because the two
/// numbers live in different modules and neither knows the other exists.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP client every provider should use.
///
/// WHY THIS EXISTS RATHER THAN `reqwest::Client::new()` AT EACH PROVIDER.
/// reqwest keeps an idle connection in its pool for 90 seconds by default
/// (verified at source, reqwest 0.12.28 `async_impl/client.rs`), and this module
/// re-fetches every unit every `BASE_INTERVAL` -- 60 seconds. SIXTY IS LESS THAN
/// NINETY, so a pooled connection is handed out again before it can ever age
/// out. It is never idle long enough to be evicted.
///
/// That is harmless until a connection DIES WITHOUT SAYING SO. Measured
/// 2026-08-19: the host entered sleep at 11:07:58 and the first fetch failure
/// landed at 11:08:14, sixteen seconds later. Every provider failed at once --
/// not because they share a client, but because every pool held sockets opened
/// before that sleep. The refresher then reused those dead sockets every 60
/// seconds for TEN HOURS, each attempt refreshing the very idle timer that
/// needed to expire for them to be discarded. A fresh process on the same host
/// fetched all 37 providers in twenty seconds; a restart cured it.
///
/// So the pool idle timeout must sit BELOW the refresh cadence. That trades
/// cross-tick connection reuse -- one TLS handshake per provider per minute,
/// which is nothing for a poller -- for the removal of a whole failure class.
/// Reuse WITHIN a tick is preserved, which is the reuse that matters: providers
/// making several calls per fetch make them seconds apart.
pub fn provider_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        // A peer that dies without sending a FIN is exactly what host sleep
        // leaves behind. Keepalive gives the kernel a way to notice, rather than
        // leaving detection entirely to the idle timeout above.
        .tcp_keepalive(POOL_IDLE_TIMEOUT)
        .build()
        // A builder failure means the TLS backend could not initialise, which no
        // provider can do anything about. Falling back keeps the process serving
        // rather than turning every lane into a startup panic -- it loses the
        // pool bound, and a bounded outage that heals on restart beats a module
        // that will not start.
        .unwrap_or_else(|_| reqwest::Client::new())
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

    /// A POST with NO default `Accept`/`Content-Type` headers, for non-JSON wire
    /// protocols (e.g. grok's gRPC-web, where the caller sets
    /// `application/grpc-web+proto` itself and a JSON `Accept` changes the server's
    /// response framing). The caller supplies all content headers via `.header(..)`.
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::Post(body),
            url: url.into(),
            headers: vec![],
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
    /// - 2xx with an empty body → [`FetchError::Upstream`] (see below)
    pub async fn send(self, client: &reqwest::Client) -> Result<Vec<u8>, FetchError> {
        // The empty-body rule is applied by `send_full`, which every classifying
        // helper routes through.
        Ok(self.send_full(client).await?.body)
    }

    /// Like [`send`](Self::send) but also returns the response status + headers
    /// (2xx only), for providers whose window lives in a header (e.g.
    /// `x-ratelimit-reset-*`). Same uniform error mapping as [`send`](Self::send).
    pub async fn send_full(self, client: &reqwest::Client) -> Result<HttpResponse, FetchError> {
        let raw = self.send_raw(client).await?;
        if raw.status == 401 || raw.status == 403 {
            // EXCERPTED LIKE EVERY OTHER NON-2XX, which it was not until now, and
            // the omission was exactly backwards. Every other refusal carried 200
            // characters of the upstream's own words; the one class whose remedy is
            // an OPERATOR ACTION carried `HTTP 403` and nothing else.
            //
            // A 403 is not reliably an auth verdict. Two live rows on this host say
            // so: `gemini` answers `unauthorized: quota: HTTP 403` because Google
            // sunset Code Assist for individual accounts, where re-authenticating
            // cannot help, and upstream CodexBar hit the same shape from a different
            // direction at v0.56.4 -- claude.ai returning 403 for a Cloudflare
            // challenge, which they had to add a distinct class for because the
            // remedy is a network change rather than a login.
            //
            // The class stays `Unauthorized` rather than growing a sibling. A
            // challenge-versus-expiry predicate would be a guess about pages we have
            // never observed, and the honest fix is to carry the evidence rather
            // than to classify it: an operator reading a Cloudflare interstitial or
            // Google's own sunset text in `ProviderUsage.error` can tell what
            // happened, and no wire change is needed for them to do it.
            //
            // THIS EXTENDS THE UNREDACTED-EXCERPT REVIEW POINT BELOW TO 401/403
            // BODIES. Refusal bodies are the likeliest place for a provider to echo
            // the rejected credential back, so the obligation stated there -- check
            // what a new provider's non-2xx bodies contain -- now covers the auth
            // statuses too, and matters more there.
            let excerpt: String = String::from_utf8_lossy(&raw.body)
                .chars()
                .take(200)
                .collect();
            let detail = if excerpt.trim().is_empty() {
                // An empty refusal body is a FACT worth stating rather than a blank
                // to hide: it tells a reader the upstream said nothing, which is
                // different from nobody having looked.
                format!("HTTP {} (no response body)", raw.status)
            } else {
                format!("HTTP {}: {excerpt}", raw.status)
            };
            return Err(FetchError::Unauthorized(detail));
        }
        if !(200..300).contains(&raw.status) {
            // THE EXCERPT IS UNREDACTED BY CONSTRUCTION, and it reaches the wire
            // in `ProviderUsage.error`. Every other leak class here is closed --
            // URLs are stripped, credential paths are replaced by descriptions,
            // lengths are bounded -- but this path echoes 200 characters of
            // whatever the upstream sent, which is what makes it useful for
            // diagnosis and also what makes it the one uninspected channel.
            //
            // No provider currently returns a secret in an error body. The hazard
            // is that a SUCCESS payload can carry one and nobody would look here:
            // minimax's `token_plan_credit` response includes a live `api_key`
            // field (reported on insula#2, 2026-08-17). That endpoint signals
            // app-level errors as `base_resp.status_code` on HTTP 200, so its
            // failures are parsed rather than excerpted and it does not leak
            // today -- but a provider that carried a secret in a payload AND
            // answered non-2xx with it would, silently.
            //
            // Deliberately not scrubbed with a key-shaped heuristic: it would
            // mangle genuine diagnostics far more often than it would catch a
            // secret, and a scrubber that mostly fires on false positives trains
            // people to distrust the field. The rule instead is a REVIEW POINT --
            // when adding a provider whose payload carries credentials, check
            // what its non-2xx bodies contain before routing it through here.
            let excerpt: String = String::from_utf8_lossy(&raw.body)
                .chars()
                .take(200)
                .collect();
            return Err(FetchError::Upstream(format!(
                "HTTP {}: {excerpt}",
                raw.status
            )));
        }
        raw.body_for_parsing()?;
        Ok(raw)
    }

    /// Vault requests retain the HTTP status even when a rejected response body is
    /// truncated. Successful bodies remain mandatory; rejected bodies are
    /// diagnostic-only and are consumed on a best-effort basis.
    pub(crate) async fn send_provider_status_first(
        self,
        client: &reqwest::Client,
        provider: &'static str,
    ) -> Result<HttpResponse, FetchError> {
        let mut builder = match &self.method {
            Method::Get => client.get(&self.url),
            Method::Post(body) => client.post(&self.url).body(body.clone()),
        }
        .timeout(self.timeout);
        for header in &self.headers {
            builder = builder.header(header.name, &header.value);
        }

        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            if read_error_body_prefix(response).await.is_err() {
                eprintln!(
                    "[ck-quota] warning: {provider} rejected-response body was incomplete status={status}"
                );
            }
            return Err(FetchError::ProviderStatus(status));
        }
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let body = read_success_body(response).await?;
        let response = HttpResponse {
            status,
            headers,
            body,
        };
        // Applied here as well as in `send_full`: this helper builds its own
        // response rather than routing through that one, so a rule added there
        // does not reach these callers. Every caller of this method parses the
        // body, so none of them wants an empty one.
        response.body_for_parsing()?;
        Ok(response)
    }

    /// Execute the request and return the raw status + headers + body with NO
    /// status-based error mapping (only transport/timeout → `Upstream`). For the
    /// rare provider whose status semantics are bespoke — e.g. doubao must read
    /// rate-limit headers off BOTH 200 AND 429 — so it owns the status policy while
    /// still riding this shared transport instead of hand-rolling `reqwest`.
    ///
    /// The empty-body rule that [`send`](Self::send) applies is **deliberately
    /// absent here**, because a caller reading rate-limit headers off a `429`
    /// never looks at the body and would break if an empty one were refused. A
    /// caller that does parse the body should ask for the check explicitly via
    /// [`HttpResponse::body_for_parsing`], which carries the full reasoning.
    /// Skipping it silently is how a flapping endpoint comes to read as dead.
    pub async fn send_raw(self, client: &reqwest::Client) -> Result<HttpResponse, FetchError> {
        let mut builder = match &self.method {
            Method::Get => client.get(&self.url),
            Method::Post(body) => client.post(&self.url).body(body.clone()),
        }
        .timeout(self.timeout);
        for header in &self.headers {
            builder = builder.header(header.name, &header.value);
        }

        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = if (200..300).contains(&status) {
            read_success_body(response).await?
        } else {
            read_error_body_prefix(response).await?
        };

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before completing its request");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    async fn serve_fixed(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            let _ = stream.write_all(&body).await;
        });
        (format!("http://{address}/response"), server)
    }

    #[tokio::test]
    async fn normal_success_body_is_returned_byte_for_byte() {
        let expected = vec![0, 1, b'{', b'}', b'\n', 0xff];
        let (url, server) = serve_fixed(200, expected.clone()).await;

        let actual = JsonRequest::get(url)
            .send(&reqwest::Client::new())
            .await
            .unwrap();

        assert_eq!(actual, expected);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_success_body_is_transient_so_a_flap_keeps_the_cached_window() {
        // An edge that returns 200 with nothing is flapping, not answering. Every
        // normalizer would fail to parse an empty body and report Decode, which
        // `refresh::classify` treats as non-transient and which therefore replaces
        // the last healthy window with a degraded entry. Classifying it Upstream
        // here is what keeps a flapping provider readable instead of dead.
        let (url, server) = serve_fixed(200, Vec::new()).await;

        let error = JsonRequest::get(url)
            .send(&reqwest::Client::new())
            .await
            .expect_err("an empty success body must not be handed to a normalizer");

        assert!(
            matches!(error, FetchError::Upstream(_)),
            "an empty 2xx must be Upstream, got {error:?}"
        );
        assert_eq!(
            crate::refresh::classify(&error),
            crate::refresh::FetchClass::Transient,
            "an empty 2xx must be transient so the refresher keeps serving the last healthy window"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn raw_send_returns_an_empty_success_body_unclassified() {
        // `send_raw` is the escape hatch for callers whose status/body policy is
        // bespoke, so it must not apply the empty-body rule: a caller reading a
        // header off a body-less response is entitled to that response.
        let (url, server) = serve_fixed(200, Vec::new()).await;

        let response = JsonRequest::get(url)
            .send_raw(&reqwest::Client::new())
            .await
            .expect("send_raw must not classify an empty body");

        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn declared_oversized_success_is_rejected_before_body_read() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let declared_length = MAX_RESPONSE_BODY_BYTES + 1;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {declared_length}\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let result = JsonRequest::get(format!("http://{address}/oversized"))
            .timeout(Duration::from_millis(200))
            .send(&reqwest::Client::new())
            .await;
        server.abort();

        match result {
            Err(FetchError::Decode(message)) => assert_eq!(
                message,
                format!("HTTP response body exceeds {MAX_RESPONSE_BODY_BYTES}-byte safety limit")
            ),
            Err(other) => panic!("expected Decode, got {other:?}"),
            Ok(_) => panic!("oversized declared body unexpectedly succeeded"),
        }
    }

    #[tokio::test]
    async fn chunked_oversized_success_is_rejected_while_streaming() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let chunk = vec![b'x'; 64 * 1024];
            let mut remaining = MAX_RESPONSE_BODY_BYTES + 1;
            while remaining > 0 {
                let length = remaining.min(chunk.len());
                let frame = format!("{length:X}\r\n");
                if stream.write_all(frame.as_bytes()).await.is_err()
                    || stream.write_all(&chunk[..length]).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
                remaining -= length;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            JsonRequest::get(format!("http://{address}/chunked")).send(&reqwest::Client::new()),
        )
        .await
        .expect("oversized chunked response should be rejected promptly");

        assert!(
            matches!(result, Err(FetchError::Decode(_))),
            "expected Decode, got {result:?}"
        );
        server.await.unwrap();
    }

    /// A refusal carries the upstream's own words, not just its number.
    ///
    /// 401/403 used to be the ONE non-2xx class stripped of its body, which was
    /// exactly backwards: every status whose remedy is "wait" carried 200
    /// characters of explanation, and the status whose remedy is an OPERATOR
    /// ACTION carried `HTTP 403`.
    ///
    /// A 403 is not reliably an auth verdict, and both known counterexamples cost
    /// someone real time. `gemini` answers 403 on this host because Google sunset
    /// Code Assist for individual accounts -- re-authenticating cannot fix it --
    /// and upstream CodexBar found claude.ai answering 403 for a Cloudflare
    /// challenge, where the remedy is a network change. Neither is distinguishable
    /// from an expired session by the status alone, and the wire published nothing
    /// else.
    ///
    /// The class deliberately stays `Unauthorized`: a challenge-versus-expiry
    /// predicate would be a guess about pages nobody here has observed, and
    /// carrying the evidence lets a reader decide without one.
    #[tokio::test]
    async fn an_auth_refusal_carries_its_body_so_a_challenge_is_distinguishable() {
        let body = b"<html><title>Just a moment...</title>cf-challenge</html>".to_vec();
        let (url, server) = serve_fixed(403, body).await;

        let error = JsonRequest::get(url)
            .send(&reqwest::Client::new())
            .await
            .unwrap_err();

        match error {
            FetchError::Unauthorized(message) => {
                assert!(
                    message.contains("cf-challenge"),
                    "the upstream's own words are what separate a challenge from an \
                     expired session; got {message:?}"
                );
                assert!(
                    message.contains("HTTP 403"),
                    "the status is still stated: got {message:?}"
                );
            }
            other => panic!("a 403 must stay an auth refusal, not become {other:?}"),
        }
        server.await.unwrap();
    }

    /// An empty refusal body says so rather than reading as unexamined.
    ///
    /// The pair to the test above, and the reason the message is not just a bare
    /// status: "the upstream sent nothing" and "nobody looked" are different facts,
    /// and a reader who has learned that a 403 usually explains itself needs to see
    /// which one this was.
    #[tokio::test]
    async fn an_empty_auth_refusal_states_that_the_body_was_empty() {
        let (url, server) = serve_fixed(401, Vec::new()).await;

        let error = JsonRequest::get(url)
            .send(&reqwest::Client::new())
            .await
            .unwrap_err();

        assert!(
            matches!(&error, FetchError::Unauthorized(m) if m == "HTTP 401 (no response body)"),
            "an absent body is a stated fact, not a blank: got {error:?}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_success_excerpt_is_unchanged_when_drain_is_bounded() {
        let mut body = "é".repeat(250).into_bytes();
        body.resize(ERROR_BODY_PREFIX_BYTES * 2, b'x');
        let (url, server) = serve_fixed(500, body).await;

        let result = JsonRequest::get(url).send(&reqwest::Client::new()).await;

        match result {
            Err(FetchError::Upstream(message)) => {
                assert_eq!(message, format!("HTTP 500: {}", "é".repeat(200)));
            }
            Err(other) => panic!("expected Upstream, got {other:?}"),
            Ok(_) => panic!("non-success response unexpectedly succeeded"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn small_error_body_is_drained_for_connection_reuse() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 500 Error\r\ncontent-length: 4\r\n\r\noops")
                .await
                .unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let client = reqwest::Client::new();
        let first = JsonRequest::get(format!("http://{address}/first"))
            .send(&client)
            .await;
        assert!(matches!(first, Err(FetchError::Upstream(_))));

        let second = tokio::time::timeout(
            Duration::from_secs(1),
            JsonRequest::get(format!("http://{address}/second")).send(&client),
        )
        .await
        .expect("fully drained error body should leave the connection reusable")
        .unwrap();
        assert_eq!(second, b"ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_status_mapping_survives_bounded_error_drain() {
        let body = vec![b'x'; ERROR_BODY_PREFIX_BYTES * 2];
        let (url, server) = serve_fixed(429, body).await;

        let result = JsonRequest::get(url)
            .send_provider_status_first(&reqwest::Client::new(), "test")
            .await;

        match result {
            Err(FetchError::ProviderStatus(429)) => {}
            Err(other) => panic!("expected ProviderStatus(429), got {other:?}"),
            Ok(_) => panic!("non-success response unexpectedly succeeded"),
        }
        server.await.unwrap();
    }

    /// The `error` text of a degraded entry is published to consumers, so a
    /// transport failure must not carry the request URL into it: a credential
    /// in a query parameter prints verbatim there.
    ///
    /// The request here cannot connect, which is the failure that produces a
    /// transport error with a URL attached. The userinfo assertion is a
    /// belt-and-braces check -- reqwest strips userinfo from the URL it keeps,
    /// so that half is already unreachable today.
    /// A success response carrying no body is a flap, not a parse failure.
    ///
    /// `send_raw` deliberately classifies nothing, so its callers each decide
    /// what a status and body mean. Left alone, an empty body reaches a
    /// normalizer, fails to parse, and becomes `Decode` -- non-transient, which
    /// discards a healthy cached window and reports the provider as dead. The
    /// classification here is the whole point of the helper, so it is asserted
    /// rather than just the error.
    #[test]
    fn an_empty_success_body_is_transient_when_it_is_about_to_be_parsed() {
        let response = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        };

        let error = response
            .body_for_parsing()
            .expect_err("an empty success body must not reach a parser");
        assert!(matches!(error, FetchError::Upstream(_)), "{error:?}");
        assert_eq!(
            crate::refresh::classify(&error),
            crate::refresh::FetchClass::Transient,
            "an empty body must keep the cached window, not degrade the slot"
        );

        // Not vacuous: a body that is present is returned untouched, so the
        // helper cannot pass by refusing everything.
        let ok = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: b"{}".to_vec(),
        };
        assert_eq!(ok.body_for_parsing().unwrap(), b"{}");
    }

    /// The refusal is scoped to success responses.
    ///
    /// A caller reaches this only after its own status mapping has run, so an
    /// empty body on a non-success status has already been accounted for -- and
    /// refusing it here would overwrite that caller's more specific error with a
    /// generic transport one.
    #[test]
    fn an_empty_body_on_a_non_success_status_is_left_to_the_caller() {
        let response = HttpResponse {
            status: 503,
            headers: Vec::new(),
            body: Vec::new(),
        };

        assert_eq!(response.body_for_parsing().unwrap(), b"");
    }

    /// Every helper that classifies a response applies the empty-body rule.
    ///
    /// Two of them build a response themselves rather than routing through one
    /// point, so a rule added to either does not reach the other's callers. This
    /// drives both against the same server and asserts the same classification,
    /// so the pair cannot drift apart.
    ///
    /// The pairing matters concretely: the banked-reset credit request chooses
    /// its helper by whether a vault credential is in play, so without this the
    /// same request would treat an empty body as a transient flap on one lane
    /// and as a degrading parse failure on the other.
    #[tokio::test]
    async fn every_classifying_helper_treats_an_empty_success_body_alike() {
        for label in ["send_full", "send_provider_status_first"] {
            let (url, server) = serve_fixed(200, Vec::new()).await;
            let request = JsonRequest::get(url);
            let client = reqwest::Client::new();

            let outcome = match label {
                "send_full" => request.send_full(&client).await.err(),
                _ => request
                    .send_provider_status_first(&client, "test")
                    .await
                    .err(),
            };
            let Some(error) = outcome else {
                panic!("{label} accepted an empty success body");
            };

            assert!(
                matches!(error, FetchError::Upstream(_)),
                "{label}: {error:?}"
            );
            assert_eq!(
                crate::refresh::classify(&error),
                crate::refresh::FetchClass::Transient,
                "{label} would degrade the slot instead of keeping the cached window"
            );
            server.await.unwrap();
        }
    }

    /// A body that is present still reaches the caller through both helpers, so
    /// the rule above cannot pass by refusing everything.
    #[tokio::test]
    async fn every_classifying_helper_returns_a_present_body() {
        for label in ["send_full", "send_provider_status_first"] {
            let (url, server) = serve_fixed(200, b"{}".to_vec()).await;
            let request = JsonRequest::get(url);
            let client = reqwest::Client::new();

            let response = match label {
                "send_full" => request.send_full(&client).await,
                _ => request.send_provider_status_first(&client, "test").await,
            };
            let response =
                response.unwrap_or_else(|error| panic!("{label} rejected a real body: {error:?}"));

            assert_eq!(response.body, b"{}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_transport_failure_does_not_publish_the_request_url() {
        // Port 1 on loopback refuses immediately, so this fails in `send`
        // rather than anywhere that would bypass the mapping under test.
        let url = "http://user:tok_secret_value@127.0.0.1:1/usage?api_key=key_secret_value";

        let error = JsonRequest::get(url)
            .send(&reqwest::Client::new())
            .await
            .expect_err("connecting to a closed port must fail");

        let text = error.to_string();
        assert!(
            !text.contains("tok_secret_value"),
            "userinfo leaked: {text}"
        );
        assert!(!text.contains("key_secret_value"), "query leaked: {text}");
        assert!(!text.contains("127.0.0.1"), "host leaked: {text}");
        // Not vacuous: the error still describes the failure, so this cannot
        // pass by producing an empty string.
        assert!(matches!(error, FetchError::Upstream(_)));
        assert!(!text.is_empty());
        assert!(text.contains("error sending request"), "unexpected: {text}");
    }
}
