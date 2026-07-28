//! The provider fetcher abstraction.
//!
//! Each provider exposes credential handles as independent fetch units. The
//! background refresher owns labeling and serving entries, while providers only
//! resolve a credential, observe its account identity, and normalize its usage.

use async_trait::async_trait;

use crate::credential_source::{VaultCapability, VaultGetError};
use crate::model::{AccountInfo, ProviderUsage, SavedResets, Usage};

/// Stable identity for one credential fetch unit.
///
/// A vault handle includes the exact capability snapshot used by the fetch. The
/// capability participates in equality and hashing, while all formatting exposes
/// only the non-secret credential id.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum CredentialHandle {
    ImplicitLocal,
    Named(String),
    Vault {
        credential_id: String,
        capability: VaultCapability,
    },
}

impl CredentialHandle {
    /// Build a provider-defined stable local handle.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        if id.is_empty() || id == "implicit-local" {
            Self::ImplicitLocal
        } else {
            Self::Named(id)
        }
    }

    pub fn implicit() -> Self {
        Self::ImplicitLocal
    }

    pub fn vault(credential_id: impl Into<String>, capability: VaultCapability) -> Self {
        Self::Vault {
            credential_id: credential_id.into(),
            capability,
        }
    }

    pub fn stable_id(&self) -> &str {
        match self {
            Self::ImplicitLocal => "implicit-local",
            Self::Named(id) => id,
            Self::Vault { credential_id, .. } => credential_id,
        }
    }

    pub fn is_local(&self) -> bool {
        !matches!(self, Self::Vault { .. })
    }

    pub fn vault_capability(&self) -> Option<&VaultCapability> {
        match self {
            Self::Vault { capability, .. } => Some(capability),
            Self::ImplicitLocal | Self::Named(_) => None,
        }
    }

    pub fn sort_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.stable_id().cmp(other.stable_id())
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImplicitLocal => formatter.write_str("CredentialHandle::ImplicitLocal"),
            Self::Named(id) => formatter
                .debug_tuple("CredentialHandle::Named")
                .field(id)
                .finish(),
            Self::Vault { credential_id, .. } => formatter
                .debug_struct("CredentialHandle::Vault")
                .field("credential_id", credential_id)
                .field("capability", &"<redacted>")
                .finish(),
        }
    }
}

impl std::fmt::Display for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.stable_id())
    }
}

/// A failed local handle-set read or parse.
///
/// This is distinct from an authoritative empty set so a transient config error
/// cannot accidentally reap every account for a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlesError(String);

impl HandlesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for HandlesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HandlesError {}

/// Account identity observed while resolving one credential.
///
/// `record_version` is reserved for versioned credential stores. Local sources
/// leave it absent and re-resolve the observation on every fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountObservation {
    pub account_id: Option<String>,
    pub record_version: Option<u64>,
}

impl AccountObservation {
    pub fn new(account_id: Option<String>, record_version: Option<u64>) -> Self {
        Self {
            account_id,
            record_version,
        }
    }
}

/// Whether this tick verified the identity behind the credential handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialResolution {
    Verified,
    Unverified,
}

/// One handle-scoped fetch result.
///
/// The credential observation is independent of `usage`, so an account swap is
/// still visible when the upstream usage request fails.
#[derive(Debug)]
pub struct FetchAttempt {
    pub observed: Option<AccountObservation>,
    pub source: Option<String>,
    pub usage: Result<Usage, FetchError>,
    /// Optional provider/account labels attached to a successful usage fetch.
    pub account_info: Option<AccountInfo>,
    /// Optional read-only reset inventory attached to a successful usage fetch.
    pub saved_resets: Option<SavedResets>,
    /// The slot may relax raw percentages only while this success remains fresh.
    pub relax_eligible: bool,
    pub credential_resolution: CredentialResolution,
}

impl FetchAttempt {
    pub fn success(
        observed: Option<AccountObservation>,
        source: impl Into<String>,
        usage: Usage,
    ) -> Self {
        Self {
            observed,
            source: Some(source.into()),
            usage: Ok(usage),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Verified,
        }
    }

    pub fn failure(
        observed: Option<AccountObservation>,
        source: Option<String>,
        error: FetchError,
    ) -> Self {
        Self {
            observed,
            source,
            usage: Err(error),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Verified,
        }
    }

    pub fn unverified_vault_failure(error: VaultGetError) -> Self {
        let fetch_error = match error {
            VaultGetError::Transient => {
                FetchError::Upstream("credential vault temporarily unavailable".to_string())
            }
            // The vault holds a record for this handle in both cases -- it was
            // minted deliberately -- so neither is an absent credential. One
            // needs a login, the other cannot be served at all.
            VaultGetError::AuthRequired => {
                FetchError::CredentialUnusable("credential requires authentication".to_string())
            }
            VaultGetError::Permanent => {
                FetchError::CredentialUnusable("vault credential is unavailable".to_string())
            }
            VaultGetError::FailClosed => {
                FetchError::Decode("credential vault rejected the request".to_string())
            }
        };
        Self {
            observed: None,
            source: None,
            usage: Err(fetch_error),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Unverified,
        }
    }

    /// Attach account labels discovered while resolving the credential.
    pub fn with_account_info(mut self, account_info: Option<AccountInfo>) -> Self {
        self.account_info = account_info.filter(|info| !info.is_empty());
        self
    }

    /// Attach a successful read-only reset inventory without changing mutation policy.
    pub fn with_saved_resets(mut self, saved_resets: Option<SavedResets>) -> Self {
        self.saved_resets = saved_resets;
        self
    }

    /// Set read-time relaxation eligibility without changing other providers'
    /// constructor call sites.
    pub fn with_relax_eligible(mut self, relax_eligible: bool) -> Self {
        self.relax_eligible = relax_eligible;
        self
    }

    /// Adapt an existing single-credential provider body while preserving its
    /// normalization and error mapping. The refresher still rebuilds the served
    /// entry from this envelope.
    pub fn from_provider_usage(result: Result<ProviderUsage, FetchError>) -> Self {
        match result {
            Ok(entry) => {
                // `Some(AccountObservation { account_id: None, .. })` means the
                // credential was resolved and explicitly exposes no account label.
                // Outer `None` is reserved for an unavailable observation.
                let observed = Some(AccountObservation::new(entry.account, None));
                let source = entry.source;
                let account_info = entry.account_info;
                let saved_resets = entry.saved_resets;
                let usage = entry.usage.ok_or_else(|| {
                    FetchError::Decode("provider returned a healthy entry without usage".into())
                });
                Self {
                    observed,
                    source,
                    usage,
                    account_info,
                    saved_resets,
                    relax_eligible: false,
                    credential_resolution: CredentialResolution::Verified,
                }
            }
            Err(error) => Self::failure(None, None, error),
        }
    }
}

/// A provider's handle enumeration and handle-scoped usage fetch.
#[async_trait]
pub trait UsageProvider: Send + Sync {
    /// Stable provider name (for example, `codex`).
    fn name(&self) -> &str;

    /// Read the provider's known credential handles from local configuration.
    /// This must never perform network I/O. Providers with one existing local
    /// credential source use the default implicit handle.
    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![CredentialHandle::implicit()])
    }

    /// Resolve and fetch one credential handle.
    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt;

    /// Whether this provider authenticates via a scraped local browser cookie.
    fn is_cookie_based(&self) -> bool {
        false
    }
}

/// Why a provider could not produce usage. Always foldable into a degraded
/// entry; never aborts the response array.
///
/// The variants divide on a question a consumer needs answered and cannot ask
/// any other way: **was a credential found?** A provider nobody configured on
/// this host is a permanent, correct state with nothing to fix, while a
/// provider whose credential was found and then failed is the only kind that
/// can indicate something broke. Both used to be [`Self::NoSession`], so both
/// rendered as "not connected" and the second was invisible.
///
/// Keep the distinction honest at each construction site: choose the variant by
/// what was actually established there, not by which message reads best.
#[derive(Debug)]
pub enum FetchError {
    /// No credential was found at all: the environment variable is unset, the
    /// file is absent, the browser holds no cookie. Nothing is broken and
    /// nothing is actionable.
    NoSession(String),
    /// A credential was found but cannot be used as it stands -- incomplete,
    /// empty, malformed, or refused by the credential store. Distinct from
    /// [`Self::NoSession`] because someone configured this and it needs
    /// fixing, and from [`Self::Unauthorized`] because no upstream rejected it:
    /// the problem is visible locally.
    CredentialUnusable(String),
    /// The credential works and the account genuinely has no quota to report
    /// (no plan, or a plan that publishes no windows). Not a failure: there is
    /// nothing to fix and nothing to alert on.
    NoQuotaReported(String),
    /// The session exists but is expired or rejected (401/403).
    Unauthorized(String),
    /// Numeric provider HTTP status retained for auth-failure reporting.
    ProviderStatus(u16),
    /// Transport or upstream error.
    Upstream(String),
    /// The response was not the shape we expected.
    Decode(String),
}

impl FetchError {
    /// A stable, machine-readable name for this failure, published beside the
    /// human-readable message.
    ///
    /// The message itself is prose with no stability promise -- consumers are
    /// told not to branch on it -- so without this there is no way to ask "is
    /// this provider unconfigured, or did it break?" and a provider that failed
    /// today is indistinguishable from one nobody ever set up.
    ///
    /// Derived from the variant rather than from the text, so it cannot drift
    /// from the taxonomy it names. New classes may be added: a consumer must
    /// render an unrecognised class as a degraded entry with an unknown reason
    /// rather than dropping it or folding it into any existing bucket.
    pub fn error_class(&self) -> &'static str {
        match self {
            Self::NoSession(_) => "credential_absent",
            Self::CredentialUnusable(_) => "credential_unusable",
            Self::NoQuotaReported(_) => "no_quota_reported",
            Self::Unauthorized(_) | Self::ProviderStatus(401 | 403) => "credential_rejected",
            Self::ProviderStatus(_) | Self::Upstream(_) => "upstream_failed",
            Self::Decode(_) => "decode_failed",
        }
    }
}

/// Cap on the detail carried by a published error message.
///
/// Generous enough for any message this codebase writes, and for the 200-char
/// response excerpt that non-2xx failures deliberately carry, so bounding here
/// costs no diagnostic detail in practice. It exists for the messages this
/// codebase does *not* write: a decode failure quotes the offending value
/// verbatim, and that value comes from the upstream.
const MAX_ERROR_DETAIL_BYTES: usize = 1024;

impl std::fmt::Display for FetchError {
    /// This text is published: it becomes the `error` field of a degraded entry,
    /// which consumers read and at least one stores. So the detail is bounded
    /// here, at the single point where any variant becomes wire text, rather
    /// than at each of the sites that construct one.
    ///
    /// Errors are not logged to stderr anywhere in this module, so this string
    /// is the only account of a failure -- truncation keeps the front, where
    /// the classification and the start of the detail are.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = |m: &String| crate::text::truncate_for_wire(m, MAX_ERROR_DETAIL_BYTES);
        match self {
            Self::NoSession(m) => write!(f, "no session: {}", detail(m)),
            Self::CredentialUnusable(m) => write!(f, "credential unusable: {}", detail(m)),
            Self::NoQuotaReported(m) => write!(f, "no quota reported: {}", detail(m)),
            Self::Unauthorized(m) => write!(f, "unauthorized: {}", detail(m)),
            Self::ProviderStatus(status) => write!(f, "provider returned HTTP {status}"),
            Self::Upstream(m) => write!(f, "upstream error: {}", detail(m)),
            Self::Decode(m) => write!(f, "decode error: {}", detail(m)),
        }
    }
}

impl std::error::Error for FetchError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message a decode failure carries is written by `serde_json`, not by
    /// this codebase: it quotes the value it choked on verbatim and does not
    /// bound it. Since that value comes from the upstream, the published string
    /// inherits whatever size the upstream chose.
    #[test]
    fn a_decode_failure_over_upstream_text_publishes_a_bounded_string() {
        #[derive(serde::Deserialize, Debug)]
        struct Window {
            #[allow(dead_code)]
            window_minutes: u64,
        }

        // A number was expected and a very long string arrived.
        let body = format!(r#"{{"window_minutes":"{}"}}"#, "A".repeat(200_000));
        let parse_error = serde_json::from_str::<Window>(&body)
            .expect_err("a string where a number belongs must fail to parse");

        // Pin the premise rather than assuming it: the error really does carry
        // the upstream's text, so this test would be meaningless if serde ever
        // stopped echoing it.
        let raw = parse_error.to_string();
        assert!(
            raw.len() > 100_000,
            "premise: serde echoes the value: {}",
            raw.len()
        );

        let published = FetchError::Decode(raw).to_string();

        assert!(
            published.len() < 2_000,
            "published {} bytes: the wire string is unbounded",
            published.len()
        );
        // Not vacuous: bounding must not empty the message. The classification
        // and the start of the detail survive, and the cut is announced.
        assert!(published.starts_with("decode error: invalid type"));
        assert!(published.contains("more bytes]"), "truncation is named");
    }

    /// Every variant must name a class, and the mapping is what a consumer
    /// branches on once it reaches the wire.
    ///
    /// Written as a match on a constructed value of each variant rather than a
    /// list of expected strings: adding a variant to `FetchError` fails to
    /// compile here, so a new failure kind cannot reach consumers unclassified.
    #[test]
    fn every_failure_kind_names_a_class() {
        let cases = [
            FetchError::NoSession("x".into()),
            FetchError::CredentialUnusable("x".into()),
            FetchError::NoQuotaReported("x".into()),
            FetchError::Unauthorized("x".into()),
            FetchError::ProviderStatus(401),
            FetchError::ProviderStatus(500),
            FetchError::Upstream("x".into()),
            FetchError::Decode("x".into()),
        ];

        for case in &cases {
            // The exhaustiveness fence: a new variant breaks this match, and the
            // author has to decide its class rather than inherit one.
            let expected = match case {
                FetchError::NoSession(_) => "credential_absent",
                FetchError::CredentialUnusable(_) => "credential_unusable",
                FetchError::NoQuotaReported(_) => "no_quota_reported",
                FetchError::Unauthorized(_) | FetchError::ProviderStatus(401 | 403) => {
                    "credential_rejected"
                }
                FetchError::ProviderStatus(_) | FetchError::Upstream(_) => "upstream_failed",
                FetchError::Decode(_) => "decode_failed",
            };
            assert_eq!(case.error_class(), expected, "{case:?}");
        }

        // Not vacuous: the classes genuinely divide the population, so this
        // cannot pass by mapping everything to one string.
        let distinct: std::collections::BTreeSet<_> =
            cases.iter().map(FetchError::error_class).collect();
        assert_eq!(distinct.len(), 6);

        // The load-bearing split: an absent credential and a broken one must
        // never share a class, since only the second is worth acting on.
        assert_ne!(
            FetchError::NoSession("x".into()).error_class(),
            FetchError::CredentialUnusable("x".into()).error_class(),
        );
    }

    #[test]
    fn an_ordinary_error_message_is_published_verbatim() {
        // The bound must not disturb the messages this codebase writes, which
        // are the ones a reader normally sees.
        let published = FetchError::NoSession("gemini creds have no refresh_token".into());
        assert_eq!(
            published.to_string(),
            "no session: gemini creds have no refresh_token"
        );
    }
}
