use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::codex_resets::{
    evaluate_trigger, normalize_credits, reporting_eligible, response_now, AuthFailureContext,
    ConsumeOutcome, CreditsHttpResponse, RedemptionJournal, ReqwestResetTransport, Reservation,
    ResetCoordinator, ResetRequest, ResetTickInput, ResetTransport, TriggerInput, UsageFacts,
};
use crate::credential_source::{CredentialSource, VaultCapability, VaultCredential, VaultGetError};
use crate::model::{AccountInfo, ExtraWindow, RateWindow, Usage};
use crate::provider::{AccountObservation, FetchError, HandlesError};
use crate::refresh::{BASE_INTERVAL, FRESH_HORIZON};
use crate::vault_handles::VaultHandleLoader;

fn handle(id: &str) -> CredentialHandle {
    CredentialHandle::new(id)
}

fn observed(account: Option<&str>) -> Option<AccountObservation> {
    Some(AccountObservation::new(
        account.map(ToString::to_string),
        None,
    ))
}

async fn tick(registry: &Registry) {
    registry.refresh_tick(&CancellationToken::new()).await;
}

fn force_due(registry: &Registry, provider: &str) {
    let mut store = registry.store.lock().unwrap();
    for (key, mut slot) in store.snapshot() {
        if key.provider != provider {
            continue;
        }
        slot.next_due_at = Instant::now();
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }
}

fn slot(registry: &Registry, provider: &str, handle_id: &str) -> ProviderSlot {
    let key = SlotKey::new(provider, handle(handle_id));
    registry.store.lock().unwrap().get(&key).unwrap().clone()
}

struct StubProvider {
    name: &'static str,
    cookie: bool,
    ok: bool,
}

#[async_trait]
impl UsageProvider for StubProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn is_cookie_based(&self) -> bool {
        self.cookie
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.ok {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::NoSession("stub degraded".into()))
        }
    }
}

fn registry(specs: &[(&'static str, bool, bool)]) -> Registry {
    Registry::new(
        specs
            .iter()
            .map(|&(name, cookie, ok)| {
                Box::new(StubProvider { name, cookie, ok }) as Box<dyn UsageProvider>
            })
            .collect(),
    )
}

struct BlockingProvider {
    name: &'static str,
    started: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for BlockingProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.started.notify_one();
        self.gate.notified().await;
        FetchAttempt::success(None, "test", Usage::default())
    }
}

struct RelaxThenBlockProvider {
    calls: AtomicUsize,
    started: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for RelaxThenBlockProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return FetchAttempt::success(None, "test", full_usage(47.0)).with_relax_eligible(true);
        }
        self.started.notify_one();
        self.gate.notified().await;
        FetchAttempt::success(None, "test", full_usage(99.0))
    }
}

#[tokio::test]
async fn f4_in_flight_refresh_revokes_previous_relaxation() {
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(RelaxThenBlockProvider {
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    })]));
    tick(&registry).await;
    assert_eq!(primary_percent(&registry.get_usage(None).await[0]), 0.0);

    force_due(&registry, "codex");
    let refresh = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    started.notified().await;
    assert_eq!(
        primary_percent(&registry.get_usage(None).await[0]),
        47.0,
        "the raw stored value must be visible while a newer fetch is in flight"
    );
    gate.notify_one();
    refresh.await.unwrap();
}

/// A cookie provider that is not logged in and one whose live cookie was
/// rejected are different facts, and only the second is worth acting on.
///
/// The cohort count exists to say "a browser login went stale". If an absent
/// cookie counted too, the number would sit at the cohort size forever on any
/// host that does not use every one of these services -- which is every host --
/// A cookie provider drops off the stale-login list once it succeeds again.
///
/// This is what the number depends on to mean anything: it has to reach zero
/// when the logins are working, or a reader learns to ignore it.
///
/// Two independent mechanisms produce it, and either alone is enough. A
/// successful fetch clears the slot's failure class, and the list is built only
/// for providers whose every slot is degraded, so a provider serving a window is
/// never considered for it. The assertion is on the outcome rather than on
/// either mechanism, which is the level that stays true if one of them is later
/// restructured -- and it does hold: removing both together fails this test,
/// while removing either one alone does not.
#[tokio::test]
async fn a_cookie_login_leaves_the_stale_list_once_it_works_again() {
    struct RecoveringProvider {
        succeed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl UsageProvider for RecoveringProvider {
        fn name(&self) -> &str {
            "recovering-cookie"
        }
        fn is_cookie_based(&self) -> bool {
            true
        }
        async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
            if self.succeed.load(Ordering::SeqCst) {
                FetchAttempt::success(None, "web", Usage::default())
            } else {
                FetchAttempt::failure(
                    None,
                    None,
                    FetchError::Unauthorized("HTTP 401 (session expired)".into()),
                )
            }
        }
    }

    let succeed = Arc::new(AtomicBool::new(false));
    let registry = Registry::new(vec![Box::new(RecoveringProvider {
        succeed: Arc::clone(&succeed),
    })]);

    tick(&registry).await;
    assert_eq!(
        registry.health().cookie_logins_stale,
        vec!["recovering-cookie"],
        "a rejected cookie must be listed while it is failing"
    );

    // The user signs back in.
    succeed.store(true, Ordering::SeqCst);
    force_due(&registry, "recovering-cookie");
    tick(&registry).await;

    let health = registry.health();
    assert!(
        health.cookie_logins_stale.is_empty(),
        "a recovered login is still listed as stale: {:?}",
        health.cookie_logins_stale
    );
    // Not vacuous: the provider really did recover, so the list is empty because
    // the class was cleared rather than because the slot vanished.
    assert_eq!(health.fresh, 1);
    assert!(health.degraded.is_empty());
}

/// and a real stale login would move it from seven to eight.
#[tokio::test]
async fn only_a_failed_cookie_counts_as_a_stale_login() {
    struct CookieProvider {
        name: &'static str,
        error: fn() -> FetchError,
    }

    #[async_trait]
    impl UsageProvider for CookieProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn is_cookie_based(&self) -> bool {
            true
        }
        async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
            FetchAttempt::failure(None, None, (self.error)())
        }
    }

    let registry = Registry::new(vec![
        Box::new(CookieProvider {
            name: "not-logged-in",
            error: || FetchError::NoSession("no session cookie in browser".into()),
        }),
        Box::new(CookieProvider {
            name: "login-expired",
            error: || FetchError::Unauthorized("HTTP 401".into()),
        }),
        // The site is down. A visitor with a perfectly good session sees the
        // same failure, so re-authenticating would change nothing.
        Box::new(CookieProvider {
            name: "site-erroring",
            error: || FetchError::Upstream("HTTP 500".into()),
        }),
        // The credential works and the account has no plan to report on.
        Box::new(CookieProvider {
            name: "no-plan",
            error: || FetchError::NoQuotaReported("no active quota".into()),
        }),
        // Our own defect. Reporting it as a credential problem would send the
        // reader to re-authenticate an account that is fine.
        Box::new(CookieProvider {
            name: "our-bug",
            error: || FetchError::Internal("provider fetch panicked".into()),
        }),
        // A scraped page that answered 200 without usage data. Several cookie
        // providers have no explicit signed-out detection, so this is what their
        // expired session actually looks like.
        Box::new(CookieProvider {
            name: "page-unparseable",
            error: || FetchError::Decode("no usage windows in response".into()),
        }),
    ]);
    tick(&registry).await;

    let health = registry.health();
    // No provider in this fixture is serving a window, and they split by whether
    // somebody can act on the failure. Asserted as whole lists rather than by
    // membership, so a class moving between the two fails here instead of
    // passing unnoticed.
    assert_eq!(
        health.degraded,
        vec![
            "login-expired",
            "site-erroring",
            "our-bug",
            "page-unparseable"
        ]
    );
    // Never logged in, and a working credential with no plan to report: both are
    // correct states rather than faults.
    assert_eq!(health.unconfigured, vec!["not-logged-in", "no-plan"]);
    // Only the two that mean a stored login stopped working. Asserted as the
    // whole list rather than by membership, so a class wrongly joining the count
    // fails here instead of passing unnoticed.
    assert_eq!(
        health.cookie_logins_stale,
        vec!["login-expired", "page-unparseable"]
    );
    assert_eq!(health.cookie_cohort_total, 6);
}

#[tokio::test]
async fn health_reflects_provider_outcomes() {
    let registry = registry(&[
        ("codex", false, true),
        ("cursor", true, false),
        ("amp", true, true),
        ("elevenlabs", false, false),
    ]);
    assert!(registry.health().degraded.is_empty());

    tick(&registry).await;

    let health = registry.health();
    assert_eq!(health.fresh, 2);
    // Both failures here are absent credentials, which is the expected state on
    // a host that does not use those services -- so they are reported as
    // unconfigured rather than as something to act on. `degraded` staying empty
    // is the point: it is what an operator watches, and a host where nothing is
    // wrong must leave it empty however many adapters are unconfigured.
    assert!(
        health.degraded.is_empty(),
        "absent credentials must not raise the operator signal, got {:?}",
        health.degraded
    );
    assert_eq!(health.unconfigured, vec!["cursor", "elevenlabs"]);
    // Neither is a stale-login signal either: a cookie provider nobody has
    // logged into is behaving correctly. The cohort count is exercised with a
    // real failure in `only_a_failed_cookie_counts_as_a_stale_login`.
    assert!(health.cookie_logins_stale.is_empty());
    assert!(health.last_tick_age.is_some());
}

/// One broken account beside one that was never configured is a provider
/// somebody should look at.
///
/// The unconfigured bucket exists so the degraded count stays an operator
/// trigger, and it requires EVERY account to be an expected absence. Asking
/// whether ANY account is absent would move this provider out of the count --
/// which is worse than not having split at all, because the failure disappears
/// from the number an operator watches instead of merely being diluted in it.
#[tokio::test]
async fn a_broken_account_beside_an_unconfigured_one_stays_actionable() {
    struct MixedProvider;

    #[async_trait]
    impl UsageProvider for MixedProvider {
        fn name(&self) -> &str {
            "mixed"
        }

        fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
            Ok(vec![handle("never-configured"), handle("broken")])
        }

        async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
            let error = if handle.stable_id() == "broken" {
                FetchError::CredentialUnusable("token file is a directory".into())
            } else {
                FetchError::NoSession("no credential configured".into())
            };
            FetchAttempt::failure(None, None, error)
        }
    }

    let registry = Registry::new(vec![Box::new(MixedProvider) as Box<dyn UsageProvider>]);
    tick(&registry).await;

    let health = registry.health();
    assert_eq!(
        health.degraded,
        vec!["mixed"],
        "a provider with one unusable credential must stay in the operator count"
    );
    assert!(
        health.unconfigured.is_empty(),
        "unconfigured must require every account to be an expected absence, got {:?}",
        health.unconfigured
    );
}

#[tokio::test]
async fn get_usage_preserves_registry_order() {
    let registry = registry(&[
        ("codex", false, true),
        ("cursor", true, false),
        ("amp", true, true),
    ]);
    assert!(registry.get_usage(None).await.is_empty());

    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    let names: Vec<_> = usage.iter().map(|entry| entry.provider.as_str()).collect();
    assert_eq!(names, vec!["codex", "cursor", "amp"]);
    assert!(usage[0].error.is_none());
    assert!(usage[1].error.is_some());
    assert!(usage[2].error.is_none());
}

#[tokio::test]
async fn read_never_blocks_on_an_inflight_fetch() {
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    })]));
    let running = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    started.notified().await;

    let usage = tokio::time::timeout(Duration::from_millis(200), registry.get_usage(None))
        .await
        .expect("cache-only read waited for an in-flight fetch");
    assert!(usage.is_empty());

    gate.notify_one();
    running.await.unwrap();
}

/// The canonical slug must actually reach the wire, not merely exist in a table.
///
/// The neighbouring test parses the mapping out of the source text, so it stays
/// green even if nothing ever consults the map. That leaves the lookup itself
/// unproven: replacing it with one that returns nothing for every provider
/// passes, because absent is a LEGAL value here -- fourteen of this module's
/// providers have no counterpart and correctly publish no slug at all.
///
/// So a consumer joining capacity to spend on `apiProvider` would find the field
/// simply missing everywhere, which is indistinguishable from the honest case
/// and reads as "this producer has no canonical name for anything".
#[tokio::test]
async fn a_mapped_provider_publishes_its_canonical_slug() {
    // `codex` is in the map; `mock` deliberately is not, and its absence is the
    // control -- without it, a lookup returning a constant slug for everything
    // would also pass.
    let registry = registry(&[("codex", false, true), ("mock", false, true)]);
    tick(&registry).await;

    let entries = registry.get_usage(None).await;
    let slug = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.provider == name)
            .unwrap_or_else(|| panic!("{name} must be served"))
            .api_provider
            .clone()
    };

    assert_eq!(
        slug("codex").as_deref(),
        Some("openai"),
        "a mapped provider must publish its canonical slug on the wire"
    );
    assert_eq!(
        slug("mock"),
        None,
        "a provider with no counterpart must publish no slug"
    );
}

#[test]
fn every_api_provider_key_names_a_registered_provider() {
    // Read this as a check on the MAP, not on the lookup: it verifies the table is
    // well-formed by parsing the source text, which stays true even if nothing
    // ever calls the function. `a_mapped_provider_publishes_its_canonical_slug`
    // covers the other half -- that the map is actually reached and its value
    // reaches the wire.
    //
    // `apiProvider` is a claim about identity, not a label: consumers join
    // pricing and spend data on it. The mapping is keyed by this module's own
    // provider name, so renaming a provider silently strips the canonical slug
    // from its wire entries -- the stale key simply stops matching and the
    // lookup returns None, which is also what "no counterpart exists" looks
    // like. Nothing else would fail.
    //
    // Checking every key resolves to a registered provider makes that drift
    // visible at the moment of the rename.
    let registry = Registry::with_defaults(crate::config::QuotaConfig::default(), None);
    let registered: std::collections::HashSet<&str> =
        registry.provider_names().into_iter().collect();

    let source = include_str!("lib.rs");
    let start = source
        .find("fn api_provider_name")
        .expect("api_provider_name must exist");
    let body_end = source[start..]
        .find("\n}")
        .expect("api_provider_name must have a body");
    let body = &source[start..start + body_end];

    let mut keys = Vec::new();
    for line in body.lines() {
        let Some((left, right)) = line.split_once("=> Some(") else {
            continue;
        };
        let Some(key) = left.split('"').nth(1) else {
            continue;
        };
        let value = right.split('"').nth(1).unwrap_or_default().to_string();
        keys.push((key.to_string(), value));
    }

    assert!(
        keys.len() >= 15,
        "parsed too few mapping keys ({}), the extraction is broken rather than the map",
        keys.len()
    );

    let unknown: Vec<&str> = keys
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !registered.contains(key))
        .collect();
    assert!(
        unknown.is_empty(),
        "api_provider_name maps provider names that are not registered: {unknown:?} -- \
         a provider was renamed and its canonical slug silently stopped reaching the wire"
    );

    for (key, value) in &keys {
        assert!(
            !value.is_empty(),
            "provider {key} maps to an empty canonical name"
        );
    }

    // No two providers may publish the same slug. Consumers key freshness,
    // pricing and spend on `apiProvider`, so a shared value merges two different
    // products into one identity -- and the merge is silent, because both
    // entries are individually well-formed and the collision is visible only by
    // comparing them.
    //
    // The near miss this guards against is concrete. `antigravity` and `gemini`
    // are separate products reached through the same Google API, with separate
    // credentials and separate quota pools, and `gemini` already publishes
    // `google`. Giving antigravity the obvious-looking slug would make one
    // product's exhaustion read as the other's.
    let mut by_slug: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (key, value) in &keys {
        by_slug
            .entry(value.as_str())
            .or_default()
            .push(key.as_str());
    }
    let mut collisions: Vec<String> = by_slug
        .iter()
        .filter(|(_, providers)| providers.len() > 1)
        .map(|(slug, providers)| format!("{slug} <- {providers:?}"))
        .collect();
    collisions.sort();
    assert!(
        collisions.is_empty(),
        "two providers publish the same apiProvider slug, merging them into one \
         identity for every consumer that keys on it: {collisions:?}"
    );
}

/// Every cookie-cohort provider publishes the cookie source label.
///
/// `source` is observability only, but its job is to tell a reader how a figure
/// was obtained so they know what to do when it stops arriving. These providers
/// authenticate with a browser session cookie, which is fixed only by logging
/// into the site in Chrome on this machine -- a different remedy from every
/// other provider here, and the reason this cohort cannot work headless.
///
/// The label is the only field that separates them once a fetch SUCCEEDS. A
/// failure names the cookie in its error text, but a healthy entry carries no
/// other evidence of how it was authenticated.
///
/// Checked against `is_cookie_based`, which the registry already uses to size
/// its stale-login metric, so the two cannot disagree about who is in the
/// cohort. A provider added to one and not the other fails here.
/// Every shipped provider must enumerate the same handles twice in a row.
///
/// The scheduler calls `handles()` on all providers at the start of every turn
/// and treats the answer as authoritative: handles it does not name are reaped,
/// handles it names for the first time are created due-now. A provider whose
/// answer varies between identical calls therefore destroys and recreates its
/// own slot each turn -- which resets the incarnation, discards any cached
/// window, restarts backoff, and means a slow provider can never finish a fetch
/// before its slot is replaced.
///
/// Nothing else would report that. The module stays healthy and the provider
/// simply never produces a reading, which is indistinguishable from an account
/// with no credentials.
///
/// Every other test of this machinery uses purpose-built stubs, so the
/// obligation is proven about the stubs rather than about the implementations
/// that ship. This drives the real ones.
///
/// It compares consecutive calls rather than asserting a specific set: what a
/// provider finds depends on the machine, and on a host with no credentials most
/// return the same empty answer twice, which is agreement rather than vacuity.
/// `handles()` reads config and credential files only -- no network -- so calling
/// it twice is cheap.
#[test]
fn every_provider_enumerates_the_same_handles_twice() {
    let registry = Registry::with_defaults(crate::config::QuotaConfig::default(), None);
    let total = registry.provider_names().len();

    let mut compared = 0;
    for provider in &registry.providers {
        let name = provider.name.as_str();
        let (Ok(first), Ok(second)) = (provider.fetcher.handles(), provider.fetcher.handles())
        else {
            // An enumeration error is a legitimate answer -- an unreadable config
            // is retained rather than treated as authoritative-empty -- and says
            // nothing about determinism.
            continue;
        };
        let ids = |handles: Vec<CredentialHandle>| {
            let mut ids: Vec<String> = handles
                .iter()
                .map(|handle| handle.stable_id().to_string())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(
            ids(first),
            ids(second),
            "{name} enumerated different handles on two consecutive calls, so the \
             scheduler would reap and recreate its slot every turn"
        );
        compared += 1;
    }

    // Without this the test passes on a machine where every provider errors, and
    // a clean result would cover nothing at all.
    assert_eq!(
        compared, total,
        "only {compared} of {total} providers were compared, so this covers less \
         than it appears to"
    );
}

#[test]
fn every_cookie_provider_publishes_the_cookie_source_label() {
    // The cohort by behaviour, not by a list written here: taken from the same
    // predicate the health metric counts.
    let registry = Registry::with_defaults(crate::config::QuotaConfig::default(), None);
    let cookie_providers = registry.cookie_based_provider_names();

    assert!(
        cookie_providers.len() >= 5,
        "found too few cookie providers ({}), the enumeration is broken rather \
         than the cohort",
        cookie_providers.len()
    );

    // Source names differ from wire names in two places, so map rather than
    // assume: a mismatch would silently skip a provider.
    let file_of = |provider: &str| -> String {
        match provider {
            "opencodego" => "opencodego".to_string(),
            "qwen-cloud" => "qwen_cloud".to_string(),
            other => other.replace('-', "_"),
        }
    };

    let sources: std::collections::HashMap<&str, &str> = [
        ("amp", include_str!("amp.rs")),
        ("cursor", include_str!("cursor.rs")),
        ("factory", include_str!("factory.rs")),
        ("mimo", include_str!("mimo.rs")),
        ("ollama", include_str!("ollama.rs")),
        ("opencode", include_str!("opencode.rs")),
        ("opencodego", include_str!("opencodego.rs")),
        ("qoder", include_str!("qoder.rs")),
        ("qwen_cloud", include_str!("qwen_cloud.rs")),
    ]
    .into_iter()
    .collect();

    let mut wrong = Vec::new();
    for provider in &cookie_providers {
        let file = file_of(provider);
        let Some(source) = sources.get(file.as_str()) else {
            wrong.push(format!(
                "{provider}: cookie-based but this test has no source for it"
            ));
            continue;
        };
        let runtime = source.split("#[cfg(test)]").next().unwrap_or(source);
        // The healthy() constructor is where the label reaches the wire.
        for line in runtime.lines().filter(|line| line.contains("healthy(")) {
            if line.contains("SOURCE_LABEL") {
                continue;
            }
            // The call may span lines; only a literal on the same line is
            // decidable here, and that is the shape being guarded against.
            if line.contains('"') {
                wrong.push(format!("{provider}: {}", line.trim()));
            }
        }
        if !runtime.contains("SOURCE_LABEL") {
            wrong.push(format!(
                "{provider}: publishes no SOURCE_LABEL, so its healthy entries \
                 are indistinguishable from a key-based provider's"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "cookie providers must publish the shared cookie label: {wrong:#?}"
    );

    // Not vacuous: the constant must be the cookie label, so this cannot pass by
    // every provider agreeing on the wrong string.
    assert_eq!(crate::browser_cookies::SOURCE_LABEL, "cookie");
}

struct LabelProvider {
    labels: Arc<Mutex<HashMap<String, Option<String>>>>,
}

#[async_trait]
impl UsageProvider for LabelProvider {
    fn name(&self) -> &str {
        "multi"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("H1"), handle("H2")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let account = self.labels.lock().unwrap()[handle.stable_id()].clone();
        // Descriptive text always accompanies the payload, whether or not an
        // identity resolved -- which is the real upstream shape and the reason
        // the two fields are asserted independently below.
        FetchAttempt::success(
            Some(AccountObservation::new(account, None)),
            "test",
            Usage::default(),
        )
        .with_account_info(Some(AccountInfo {
            email: Some(format!("{}@example.test", handle.stable_id())),
            org_name: None,
            plan_type: None,
        }))
    }
}

/// Every slot status must be classified into a health bucket deliberately.
///
/// The identity `fresh + stale + pending + degraded + unconfigured +
/// withoutHandles == providersTotal` proves no state reports NOWHERE. It cannot
/// prove a state reports in the RIGHT place: the buckets branch on booleans
/// derived from the status, so a new variant fails every `has_*` test, falls
/// into the last arm, and the sum still balances while the provider is counted
/// as something it is not.
///
/// So the identity needs this beside it. Adding a variant to `SlotStatus` fails
/// to compile in `every()` and again in the match below, which forces an author
/// to name the bucket rather than inherit one by falling through.
#[test]
fn every_slot_status_is_deliberately_bucketed() {
    for status in SlotStatus::every() {
        // The fence: a new variant breaks this match.
        let bucket = match status {
            SlotStatus::Fresh => "fresh",
            SlotStatus::StaleTransient => "stale",
            SlotStatus::Pending => "pending",
            SlotStatus::Degraded => "degraded-or-unconfigured",
        };
        assert!(!bucket.is_empty());
    }

    // Not vacuous: the four statuses reach three distinct destinations, so this
    // cannot pass by mapping everything to one bucket.
    let distinct: std::collections::BTreeSet<_> = SlotStatus::every()
        .into_iter()
        .map(|status| match status {
            SlotStatus::Fresh => "fresh",
            SlotStatus::StaleTransient => "stale",
            SlotStatus::Pending => "pending",
            SlotStatus::Degraded => "degraded-or-unconfigured",
        })
        .collect();
    assert_eq!(distinct.len(), 4);
}

#[tokio::test]
async fn unresolved_multi_handle_provider_emits_one_unlabeled_entry_then_deduplicates() {
    let labels = Arc::new(Mutex::new(HashMap::from([
        ("H1".to_string(), None),
        ("H2".to_string(), None),
    ])));
    let registry = Registry::new(vec![Box::new(LabelProvider {
        labels: Arc::clone(&labels),
    })]);

    tick(&registry).await;
    let unresolved = registry.get_usage(None).await;
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].account, None);
    // The two fields are gated independently: `account` is the identity this
    // module verified and is withheld while any handle is unresolved, while
    // `accountInfo` is unverified descriptive text that rides along with the
    // payload. A consumer must not reconstruct the withheld identity from it,
    // so the wire has to keep offering the case where one is present without
    // the other rather than quietly suppressing it.
    assert_eq!(
        unresolved[0]
            .account_info
            .as_ref()
            .and_then(|info| info.email.as_deref()),
        Some("H1@example.test"),
        "descriptive account text must survive an unlabeled emission"
    );

    *labels.lock().unwrap() = HashMap::from([
        ("H1".to_string(), Some("A".to_string())),
        ("H2".to_string(), Some("B".to_string())),
    ]);
    force_due(&registry, "multi");
    tick(&registry).await;
    let labeled = registry.get_usage(None).await;
    assert_eq!(
        labeled
            .iter()
            .map(|entry| entry.account.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("A"), Some("B")]
    );

    labels.lock().unwrap().insert("H2".into(), Some("A".into()));
    force_due(&registry, "multi");
    tick(&registry).await;
    let deduplicated = registry.get_usage(None).await;
    assert_eq!(deduplicated.len(), 1);
    assert_eq!(deduplicated[0].account.as_deref(), Some("A"));
}

#[tokio::test]
async fn one_identity_less_handle_suppresses_the_labels_of_every_other_handle() {
    // The mixed case: one handle resolves an account and the other cannot. The
    // read path emits labeled entries only when EVERY handle resolves one, so a
    // single identity-less lane collapses the whole provider to one unlabeled
    // entry and the account that WAS resolved becomes invisible.
    //
    // The shape that reaches this state: a provider whose vault lanes carry
    // identity beside a local lane that can never resolve one. It is invisible
    // while both lanes are identity-less, and appears the moment the credential
    // store starts capturing identity for that provider — without any change
    // here.
    let labels = Arc::new(Mutex::new(HashMap::from([
        ("H1".to_string(), Some("A".to_string())),
        ("H2".to_string(), None),
    ])));
    let registry = Registry::new(vec![Box::new(LabelProvider {
        labels: Arc::clone(&labels),
    })]);

    tick(&registry).await;
    let mixed = registry.get_usage(None).await;
    assert_eq!(mixed.len(), 1, "mixed resolution must not emit two entries");
    assert_eq!(
        mixed[0].account, None,
        "one identity-less handle forces the whole provider unlabeled"
    );

    // Remove the identity-less lane and the resolved account becomes visible,
    // which is what proves the suppression above was caused by that lane rather
    // than by the label never being resolved at all.
    labels.lock().unwrap().insert("H2".into(), Some("A".into()));
    force_due(&registry, "multi");
    tick(&registry).await;
    let labeled = registry.get_usage(None).await;
    assert_eq!(labeled.len(), 1);
    assert_eq!(labeled[0].account.as_deref(), Some("A"));
}

/// A labeled provider can publish one account and omit a sibling that still
/// exists.
///
/// Both accounts resolve, so the read path takes the labeled branch and every
/// entry carries its own account. A handle whose identity then becomes
/// unconfirmed is withheld -- its entry is cleared while its observation is
/// kept, so the provider still counts as fully resolved and its remaining
/// accounts stay labeled. The result is an array naming account A with no
/// mention of account B at all.
///
/// This is the case a consumer cannot infer from the array: nothing marks the
/// response as partial, and B's absence is identical in shape to B having been
/// removed. A consumer that reconciles by provider -- deleting stored accounts
/// not named in a response that contained a usable entry -- loses B here, and
/// gets it back only if it refetches. Absence of an account is never a statement
/// that the account is gone.
#[tokio::test]
async fn a_withheld_account_leaves_its_labeled_siblings_published() {
    let labels = Arc::new(Mutex::new(HashMap::from([
        ("H1".to_string(), Some("A".to_string())),
        ("H2".to_string(), Some("B".to_string())),
    ])));
    let registry = Registry::new(vec![Box::new(LabelProvider {
        labels: Arc::clone(&labels),
    })]);

    tick(&registry).await;
    let both = registry.get_usage(None).await;
    assert_eq!(
        both.iter()
            .map(|entry| entry.account.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("A"), Some("B")],
        "both accounts must be published before one is withheld"
    );

    // Withhold H2 exactly as the fail-closed path does: clear the entry, keep
    // the observation. Keeping it is what holds the provider "fully resolved",
    // so the siblings stay labeled instead of collapsing to one unlabeled entry.
    let key = SlotKey::new("multi", handle("H2"));
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        slot.label_in_flux = true;
        slot.entry = None;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    let partial = registry.get_usage(None).await;
    assert_eq!(
        partial
            .iter()
            .map(|entry| entry.account.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("A")],
        "the withheld account must vanish from the array while its sibling stays labeled"
    );

    // Not vacuous: B is still a live account, and returns on the next successful
    // fetch without anything being re-added. If withholding had removed the
    // account rather than suppressing one response, this would stay absent.
    force_due(&registry, "multi");
    tick(&registry).await;
    let restored = registry.get_usage(None).await;
    assert_eq!(
        restored
            .iter()
            .map(|entry| entry.account.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("A"), Some("B")],
        "the withheld account must come back on its own"
    );
}

/// A provider whose handle set and per-handle accounts are both steerable, so a
/// credential can be withdrawn or an enumeration made to fail mid-test.
struct CompletenessProvider {
    name: &'static str,
    handles: Arc<Mutex<Result<Vec<CredentialHandle>, ()>>>,
    labels: Arc<Mutex<HashMap<String, Option<String>>>>,
    fail: Arc<Mutex<HashSet<String>>>,
}

impl CompletenessProvider {
    fn new(handles: &[&str], labels: &[(&str, Option<&str>)]) -> Self {
        Self::named("complete", handles, labels)
    }

    fn named(name: &'static str, handles: &[&str], labels: &[(&str, Option<&str>)]) -> Self {
        Self {
            name,
            handles: Arc::new(Mutex::new(Ok(handles
                .iter()
                .map(|id| handle(id))
                .collect()))),
            labels: Arc::new(Mutex::new(
                labels
                    .iter()
                    .map(|(id, account)| ((*id).to_string(), account.map(ToString::to_string)))
                    .collect(),
            )),
            fail: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[async_trait]
impl UsageProvider for CompletenessProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.handles
            .lock()
            .unwrap()
            .clone()
            .map_err(|()| HandlesError::new("enumeration failed"))
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let id = handle.stable_id().to_string();
        let account = self.labels.lock().unwrap().get(&id).cloned().flatten();
        if self.fail.lock().unwrap().contains(&id) {
            return FetchAttempt::failure(
                observed(account.as_deref()),
                Some("test".to_string()),
                FetchError::Unauthorized("token rejected".to_string()),
            );
        }
        FetchAttempt::success(observed(account.as_deref()), "test", Usage::default())
    }
}

fn complete_set(snapshot: &crate::UsageSnapshot) -> Vec<&str> {
    snapshot
        .complete_providers
        .iter()
        .map(String::as_str)
        .collect()
}

fn accounts_of(snapshot: &crate::UsageSnapshot) -> Vec<Option<&str>> {
    snapshot
        .entries
        .iter()
        .map(|entry| entry.account.as_deref())
        .collect()
}

/// Before the refresher's first turn, no provider is complete.
///
/// At that moment no provider has slots, so a rule keyed on "has no slots"
/// would call every provider complete-with-zero-accounts and authorise a
/// consumer to delete its entire store at every module start. Completeness is
/// keyed on a handle enumeration having SUCCEEDED, which cannot have happened
/// before the first turn.
#[tokio::test]
async fn no_provider_is_complete_before_the_first_turn() {
    let registry = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1"],
        &[("H1", Some("A"))],
    ))]);

    let cold = registry.usage_snapshot(None).await;
    assert!(cold.entries.is_empty());
    assert!(
        cold.complete_providers.is_empty(),
        "a cold registry must authorise no deletion: {:?}",
        cold.complete_providers
    );

    // Not vacuous: one turn later the same provider IS complete, so the
    // emptiness above is the pre-tick state rather than the mechanism never
    // producing anything.
    tick(&registry).await;
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["complete"]
    );
}

/// A failed enumeration withdraws the completeness claim while entries remain.
///
/// The previous handle set is deliberately retained on a failed enumeration, so
/// the entries keep serving. But a retained set may name accounts that no longer
/// exist and cannot name any that were added, so it confirms nothing about the
/// account set and must not authorise a replacement.
#[tokio::test]
async fn an_enumeration_failure_withdraws_completeness_but_keeps_the_entries() {
    let provider = CompletenessProvider::new(&["H1"], &[("H1", Some("A"))]);
    let handles = Arc::clone(&provider.handles);
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["complete"]
    );

    *handles.lock().unwrap() = Err(());
    tick(&registry).await;

    let after = registry.usage_snapshot(None).await;
    assert_eq!(
        accounts_of(&after),
        vec![Some("A")],
        "a failed enumeration must not disturb the entries"
    );
    assert!(
        after.complete_providers.is_empty(),
        "a retained handle set cannot confirm the account set: {:?}",
        after.complete_providers
    );
}

/// An account still waiting on its first fetch forfeits the claim.
///
/// Its resolved sibling stays LABELED: a handle with no entry publishes no
/// capacity, so it cannot be the same account as a labeled sibling and there is
/// nothing to double-count by keeping the label. Completeness is withheld all
/// the same, and that is the property protecting the consumer here — the array
/// is missing an account, so authorising a replacement against it would delete
/// one that exists.
#[tokio::test]
async fn a_pending_account_forfeits_the_claim() {
    let provider = CompletenessProvider::new(&["H1"], &[("H1", Some("A"))]);
    let handles = Arc::clone(&provider.handles);
    let labels = Arc::clone(&provider.labels);
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["complete"]
    );

    // A second credential appears and has not been fetched yet.
    *handles.lock().unwrap() = Ok(vec![handle("H1"), handle("H2")]);
    labels.lock().unwrap().insert("H2".into(), Some("B".into()));
    {
        let mut store = registry.store.lock().unwrap();
        store.reconcile(
            "complete",
            &[handle("H1"), handle("H2")],
            std::time::Instant::now(),
        );
    }

    let mid = registry.usage_snapshot(None).await;
    assert_eq!(
        accounts_of(&mid),
        vec![Some("A")],
        "a handle awaiting its first fetch must not cost its sibling the label"
    );
    assert!(
        mid.complete_providers.is_empty(),
        "an array missing an account must never authorise replacing them"
    );
}

/// A handle that resolves no account and serves no usage keeps its siblings labeled.
///
/// This is the state a handle reaches when the credential behind it is removed
/// and the handle is left configured: it can never resolve again, and nothing
/// about it changes on its own. Collapsing the provider for it used to cost
/// every healthy account its identity permanently and silently — the wire
/// showed one unlabeled row, every health reading said serving, and only the
/// completeness claim recorded that anything was wrong.
///
/// The label is safe to keep because the failing handle publishes no capacity:
/// a degraded entry carries an error and no usage, so it cannot be the same
/// account as a labeled sibling in the way an identity-less handle SERVING
/// usage can. That distinction is the whole rule, and its other half is pinned
/// by `one_identity_less_handle_suppresses_the_labels_of_every_other_handle`.
#[tokio::test]
async fn a_failing_identity_less_handle_does_not_unlabel_its_siblings() {
    let provider =
        CompletenessProvider::new(&["H1", "H2"], &[("H1", Some("A")), ("H2", Some("B"))]);
    let fail = Arc::clone(&provider.fail);
    let labels = Arc::clone(&provider.labels);
    let registry = Registry::new(vec![Box::new(provider)]);

    // H2's credential is removed: it resolves no identity and its fetch fails.
    fail.lock().unwrap().insert("H2".to_string());
    labels.lock().unwrap().insert("H2".into(), None);

    tick(&registry).await;
    let snapshot = registry.usage_snapshot(None).await;

    let labeled: Vec<_> = snapshot
        .entries
        .iter()
        .filter_map(|entry| entry.account.as_deref())
        .collect();
    assert_eq!(
        labeled,
        vec!["A"],
        "a dead handle must not cost its healthy sibling the account label"
    );

    // Exactly one unlabeled entry, never one per failing handle: two unlabeled
    // entries for a provider are indistinguishable from two accounts.
    let unlabeled: Vec<_> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.account.is_none())
        .collect();
    assert_eq!(
        unlabeled.len(),
        1,
        "the failure stays visible, exactly once"
    );

    // And it carries NO usage. This is what makes the mixed shape safe for a
    // consumer summing a provider's capacity: the unlabeled entry may be the
    // same account as one of the labeled ones reached by a second credential,
    // so if it ever published usage the total would count that account twice.
    // The narrower collapse rule above is only sound while this holds -- a
    // handle serving usage without an identity still collapses the provider.
    assert!(
        unlabeled[0].usage.is_none(),
        "the unlabeled representative must publish no capacity: {:?}",
        unlabeled[0].usage
    );
    assert!(
        unlabeled[0].error.is_some(),
        "and it must say why it resolved no account"
    );

    // The claim is still withheld: an account is missing from the array, so
    // authorising a replacement against it would delete one that exists.
    assert!(
        snapshot.complete_providers.is_empty(),
        "a provider missing an account must not authorise replacing its set"
    );
}

/// A withheld account forfeits the claim, even though its siblings look healthy.
///
/// This is the case no other signal covers: the siblings stay labeled, provider
/// health still reports the provider serving, and the array is indistinguishable
/// from the account having been removed. Completeness is the only thing standing
/// between that array and a consumer deleting a live account.
#[tokio::test]
async fn a_withheld_account_forfeits_the_claim() {
    let registry = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1", "H2"],
        &[("H1", Some("A")), ("H2", Some("B"))],
    ))]);

    tick(&registry).await;
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["complete"]
    );

    let key = SlotKey::new("complete", handle("H2"));
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        slot.label_in_flux = true;
        slot.entry = None;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    let withheld = registry.usage_snapshot(None).await;
    assert_eq!(
        accounts_of(&withheld),
        vec![Some("A")],
        "the withheld account vanishes while its sibling stays labeled"
    );
    assert!(
        withheld.complete_providers.is_empty(),
        "a withheld account must forfeit the claim: {:?}",
        withheld.complete_providers
    );
}

/// A provider whose upstream never reports an account is never complete.
///
/// Several providers publish a single unlabeled entry by contract, because their
/// upstream exposes no account identity at all. They never enter an
/// account-keyed store, so there is nothing to authorise and their permanent
/// absence from the claim is correct rather than a fault.
#[tokio::test]
async fn an_identity_less_provider_is_never_complete() {
    let registry = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1"],
        &[("H1", None)],
    ))]);

    tick(&registry).await;
    let snapshot = registry.usage_snapshot(None).await;

    assert_eq!(accounts_of(&snapshot), vec![None]);
    assert!(
        snapshot.complete_providers.is_empty(),
        "an identity-less provider cannot claim a complete account set"
    );
}

/// Two handles resolving one account stay complete after the dedup.
///
/// A provider can reach one account through several credentials -- a local lane
/// beside a vault lane, or two vault handles -- and those are collapsed to one
/// entry deliberately. Comparing handle count against entry count would read
/// that dedup as a missing account and suppress the claim forever, on exactly
/// the providers this mechanism exists to serve.
#[tokio::test]
async fn duplicate_handles_resolving_one_account_stay_complete() {
    let registry = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1", "H2"],
        &[("H1", Some("A")), ("H2", Some("A"))],
    ))]);

    tick(&registry).await;
    let snapshot = registry.usage_snapshot(None).await;

    assert_eq!(
        accounts_of(&snapshot),
        vec![Some("A")],
        "two handles on one account publish one entry"
    );
    assert_eq!(
        complete_set(&snapshot),
        ["complete"],
        "the dedup must not read as a missing account"
    );
}

/// A successful enumeration returning no handles is complete with no entries.
///
/// This is the only way a consumer is ever authorised to clear a provider
/// outright, and it is why completeness cannot be keyed on having at least one
/// slot: the case the signal exists for is precisely the one with nothing left
/// to point at.
#[tokio::test]
async fn a_provider_with_no_credentials_left_is_complete_with_no_entries() {
    let provider = CompletenessProvider::new(&["H1"], &[("H1", Some("A"))]);
    let handles = Arc::clone(&provider.handles);
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    assert_eq!(
        accounts_of(&registry.usage_snapshot(None).await),
        vec![Some("A")]
    );

    *handles.lock().unwrap() = Ok(Vec::new());
    tick(&registry).await;

    let cleared = registry.usage_snapshot(None).await;
    assert!(cleared.entries.is_empty(), "{:?}", accounts_of(&cleared));
    assert_eq!(
        complete_set(&cleared),
        ["complete"],
        "a provider with no credentials must be clearable, not merely silent"
    );
}

/// An account whose credential is withdrawn is dropped under a complete claim.
///
/// The whole point of the mechanism: the array names only the surviving account
/// AND says so authoritatively, which is the one situation where deleting the
/// other one is right. Without this case an implementation that never claims
/// completeness passes every other test here while doing nothing.
#[tokio::test]
async fn a_removed_account_leaves_a_complete_claim_naming_only_the_survivor() {
    let provider =
        CompletenessProvider::new(&["H1", "H2"], &[("H1", Some("A")), ("H2", Some("B"))]);
    let handles = Arc::clone(&provider.handles);
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    let before = registry.usage_snapshot(None).await;
    assert_eq!(accounts_of(&before), vec![Some("A"), Some("B")]);
    assert_eq!(complete_set(&before), ["complete"]);

    // B's credential is withdrawn and the enumeration SUCCEEDS without it.
    *handles.lock().unwrap() = Ok(vec![handle("H1")]);
    tick(&registry).await;

    let after = registry.usage_snapshot(None).await;
    assert_eq!(
        accounts_of(&after),
        vec![Some("A")],
        "the withdrawn account must be gone from the entries"
    );
    assert_eq!(
        complete_set(&after),
        ["complete"],
        "and the claim must stand, or the consumer can never remove it"
    );
}

/// A degraded account is still named, and the provider is still complete.
///
/// A degraded entry names an account that exists and is currently unusable. If a
/// consumer builds its survivor set from USABLE entries only, a complete claim
/// authorises deleting an account whose token merely expired -- the original
/// data loss wearing a different costume, and worse because the deletion now
/// looks authorised.
#[tokio::test]
async fn a_degraded_account_is_named_and_keeps_the_provider_complete() {
    let provider =
        CompletenessProvider::new(&["H1", "H2"], &[("H1", Some("A")), ("H2", Some("B"))]);
    let fail = Arc::clone(&provider.fail);
    let registry = Registry::new(vec![Box::new(provider)]);

    fail.lock().unwrap().insert("H2".to_string());
    tick(&registry).await;

    let snapshot = registry.usage_snapshot(None).await;
    assert_eq!(
        accounts_of(&snapshot),
        vec![Some("A"), Some("B")],
        "a degraded account must still be named on the wire"
    );
    let degraded = snapshot
        .entries
        .iter()
        .find(|entry| entry.account.as_deref() == Some("B"))
        .expect("B is present");
    assert!(degraded.error.is_some(), "B is degraded, not healthy");
    assert!(degraded.usage.is_none());
    assert_eq!(
        complete_set(&snapshot),
        ["complete"],
        "an unusable account does not make the account SET incomplete"
    );
}

/// Either suppression alone is enough, and together they still suppress.
///
/// Pinned as a conjunction rather than inferred from the two single cases: an
/// implementation that returns the claim when both conditions hold at once --
/// a plausible way to get a boolean expression wrong -- passes both single-cause
/// tests and fails only here.
#[tokio::test]
async fn an_enumeration_failure_and_a_withheld_account_together_still_suppress() {
    let provider =
        CompletenessProvider::new(&["H1", "H2"], &[("H1", Some("A")), ("H2", Some("B"))]);
    let handles = Arc::clone(&provider.handles);
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["complete"]
    );

    let key = SlotKey::new("complete", handle("H2"));
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        slot.label_in_flux = true;
        slot.entry = None;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }
    *handles.lock().unwrap() = Err(());
    tick(&registry).await;

    let snapshot = registry.usage_snapshot(None).await;
    assert!(
        snapshot.complete_providers.is_empty(),
        "two independent suppressions must not cancel: {:?}",
        snapshot.complete_providers
    );
}

/// The claim is scoped and ordered exactly like the entries.
///
/// A consumer reads the two together, so a claim listing a provider the entries
/// do not cover would authorise a deletion against an array that never described
/// it.
#[tokio::test]
async fn the_claim_follows_the_filter_and_the_registry_order() {
    let registry = Registry::new(vec![
        Box::new(CompletenessProvider::named(
            "zeta",
            &["H1"],
            &[("H1", Some("A"))],
        )),
        Box::new(CompletenessProvider::new(&["H1"], &[("H1", Some("A"))])),
    ]);
    tick(&registry).await;

    // Registry order, not alphabetical: `zeta` is registered first.
    assert_eq!(
        complete_set(&registry.usage_snapshot(None).await),
        ["zeta", "complete"]
    );

    let filtered = registry.usage_snapshot(Some("complete")).await;
    assert_eq!(
        complete_set(&filtered),
        ["complete"],
        "a filtered read must not claim anything about providers it excluded"
    );
}

struct UnresolvedSelectionProvider;

#[async_trait]
impl UsageProvider for UnresolvedSelectionProvider {
    fn name(&self) -> &str {
        "selection"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![
            CredentialHandle::implicit(),
            CredentialHandle::vault(
                "chatgpt:openai",
                crate::credential_source::VaultCapability::new("ckh_selection_secret"),
            ),
        ])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_local() {
            FetchAttempt::success(observed(None), "local-primary", Usage::default())
        } else {
            FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession("vault credential not found".to_string()),
            )
        }
    }
}

#[tokio::test]
async fn unresolved_selection_prefers_fresh_local_over_cold_vault() {
    assert!(
        CredentialHandle::vault(
            "chatgpt:openai",
            crate::credential_source::VaultCapability::new("ckh_sort_proof"),
        )
        .sort_cmp(&CredentialHandle::implicit())
            == std::cmp::Ordering::Less,
        "the regression must put the degraded vault slot first in stable order"
    );
    let registry = Registry::new(vec![Box::new(UnresolvedSelectionProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].source.as_deref(), Some("local-primary"));
    assert!(usage[0].error.is_none());
}

#[tokio::test]
async fn i6_pending_local_does_not_hide_degraded_vault_entry() {
    let registry = Registry::new(vec![Box::new(UnresolvedSelectionProvider)]);
    tick(&registry).await;

    let local_key = SlotKey::new("selection", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut local = store.get(&local_key).unwrap().clone();
        local.entry = None;
        local.status = refresh::SlotStatus::Pending;
        assert!(store.publish_if_current(
            &local_key,
            local.incarnation,
            local.attempt_sequence,
            local,
        ));
    }

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].error.is_some(), "degraded vault entry was hidden");
}

struct SwapFailureProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for SwapFailureProvider {
    fn name(&self) -> &str {
        "swap"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::failure(
                observed(Some("B")),
                Some("test".into()),
                FetchError::Upstream("timeout".into()),
            )
        }
    }
}

#[tokio::test]
async fn account_change_observed_on_failure_clears_old_window_and_restarts_backoff() {
    let registry = Registry::new(vec![Box::new(SwapFailureProvider {
        calls: Mutex::new(0),
    })]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("A")
    );

    force_due(&registry, "swap");
    tick(&registry).await;

    assert!(registry.get_usage(None).await.is_empty());
    let slot = slot(&registry, "swap", "implicit-local");
    assert!(slot.entry.is_none());
    assert!(slot.last_success_at.is_none());
    assert!(slot.label_in_flux);
    assert_eq!(slot.account_id(), Some("B"));
    assert_eq!(slot.retry_count, 1);
    assert!(slot.next_due_at >= slot.last_attempt_at.unwrap() + BASE_INTERVAL);
}

#[tokio::test]
async fn label_in_flux_is_unavailable_even_if_a_stale_entry_exists() {
    let registry = Registry::new(vec![Box::new(StubProvider {
        name: "flux",
        cookie: false,
        ok: true,
    })]);
    tick(&registry).await;
    assert_eq!(registry.get_usage(None).await.len(), 1);

    let key = SlotKey::new("flux", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        assert!(slot.entry.is_some());
        slot.label_in_flux = true;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    assert!(registry.get_usage(None).await.is_empty());
}

struct FencedProvider {
    handles: Arc<Mutex<Vec<CredentialHandle>>>,
    calls: AtomicUsize,
    started: [Arc<Notify>; 2],
    gates: [Arc<Notify>; 2],
}

#[async_trait]
impl UsageProvider for FencedProvider {
    fn name(&self) -> &str {
        "fenced"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(self.handles.lock().unwrap().clone())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.started[call].notify_one();
        self.gates[call].notified().await;
        let account = if call == 0 { "OLD" } else { "NEW" };
        FetchAttempt::success(observed(Some(account)), "test", Usage::default())
    }
}

#[tokio::test]
async fn late_fetch_cannot_resurrect_a_reaped_or_readded_handle() {
    let handles = Arc::new(Mutex::new(vec![handle("H")]));
    let old_started = Arc::new(Notify::new());
    let new_started = Arc::new(Notify::new());
    let old_gate = Arc::new(Notify::new());
    let new_gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(FencedProvider {
        handles: Arc::clone(&handles),
        calls: AtomicUsize::new(0),
        started: [Arc::clone(&old_started), Arc::clone(&new_started)],
        gates: [Arc::clone(&old_gate), Arc::clone(&new_gate)],
    })]));

    let old_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    old_started.notified().await;
    let old_incarnation = slot(&registry, "fenced", "H").incarnation;

    handles.lock().unwrap().clear();
    tick(&registry).await;
    assert!(registry.store.lock().unwrap().snapshot().is_empty());

    handles.lock().unwrap().push(handle("H"));
    let new_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    new_started.notified().await;
    let new_incarnation = slot(&registry, "fenced", "H").incarnation;
    assert_ne!(old_incarnation, new_incarnation);

    old_gate.notify_one();
    old_tick.await.unwrap();
    assert!(registry.get_usage(None).await.is_empty());

    new_gate.notify_one();
    new_tick.await.unwrap();
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("NEW"));
}

struct EnumerationProvider {
    handles: Arc<Mutex<Vec<CredentialHandle>>>,
    fail_enumeration: Arc<Mutex<bool>>,
    calls: Arc<Mutex<HashMap<String, usize>>>,
}

#[async_trait]
impl UsageProvider for EnumerationProvider {
    fn name(&self) -> &str {
        "enumerated"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if *self.fail_enumeration.lock().unwrap() {
            Err(HandlesError::new("temporary parse failure"))
        } else {
            Ok(self.handles.lock().unwrap().clone())
        }
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(handle.stable_id().to_string())
            .or_default() += 1;
        FetchAttempt::success(observed(Some(handle.stable_id())), "test", Usage::default())
    }
}

#[tokio::test]
async fn enumeration_error_retains_active_set_and_new_handle_is_immediately_due() {
    let handles = Arc::new(Mutex::new(vec![handle("H1")]));
    let fail_enumeration = Arc::new(Mutex::new(false));
    let calls = Arc::new(Mutex::new(HashMap::new()));
    let registry = Registry::new(vec![Box::new(EnumerationProvider {
        handles: Arc::clone(&handles),
        fail_enumeration: Arc::clone(&fail_enumeration),
        calls: Arc::clone(&calls),
    })]);
    tick(&registry).await;

    *fail_enumeration.lock().unwrap() = true;
    handles.lock().unwrap().clear();
    tick(&registry).await;
    assert_eq!(registry.get_usage(None).await.len(), 1);
    assert_eq!(calls.lock().unwrap()["H1"], 1);

    *fail_enumeration.lock().unwrap() = false;
    *handles.lock().unwrap() = vec![handle("H1"), handle("H2")];
    tick(&registry).await;
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 2);
    assert_eq!(calls.lock().unwrap()["H1"], 1);
    assert_eq!(calls.lock().unwrap()["H2"], 1);
}

struct SlowManyProvider {
    gate: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for SlowManyProvider {
    fn name(&self) -> &str {
        "slow"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok((0..12)
            .map(|index| handle(&format!("H{index:02}")))
            .collect())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let permit = self.gate.acquire().await.unwrap();
        permit.forget();
        FetchAttempt::success(None, "test", Usage::default())
    }
}

struct QuickProvider {
    called: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for QuickProvider {
    fn name(&self) -> &str {
        "quick"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_one();
        FetchAttempt::success(None, "test", Usage::default())
    }
}

#[tokio::test]
async fn round_robin_admission_and_turn_heartbeat_prevent_large_provider_starvation() {
    let gate = Arc::new(Semaphore::new(0));
    let slow_calls = Arc::new(AtomicUsize::new(0));
    let quick_calls = Arc::new(AtomicUsize::new(0));
    let quick_called = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![
        Box::new(SlowManyProvider {
            gate: Arc::clone(&gate),
            calls: Arc::clone(&slow_calls),
        }),
        Box::new(QuickProvider {
            called: Arc::clone(&quick_called),
            calls: Arc::clone(&quick_calls),
        }),
    ]));
    registry
        .store
        .lock()
        .unwrap()
        .mark_tick(Instant::now() - STALL_HORIZON - Duration::from_secs(1));

    let running = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    tokio::time::timeout(Duration::from_millis(200), quick_called.notified())
        .await
        .expect("the later provider was starved behind slow handles");
    assert_eq!(quick_calls.load(Ordering::SeqCst), 1);
    // Bound against a LITERAL, not against the constant under test. Comparing
    // to `CONCURRENCY_CAP` scales the assertion with the value it is meant to
    // bound: widening the cap to 1000 admits every one of this provider's 12
    // handles and the comparison still holds, so the test passes at any cap and
    // proves only that the number is finite.
    //
    // 8 is the shipped cap and the reason it is the right literal: this
    // provider offers 12 handles, so a cap that admits all of them starves the
    // second provider entirely -- which is the behaviour this test exists to
    // catch, and it is invisible to a self-referential bound.
    assert!(
        slow_calls.load(Ordering::SeqCst) < 8,
        "one provider's handles must not fill the whole turn"
    );
    assert!(!registry.health().refresher_stalled);

    gate.add_permits(refresh::CONCURRENCY_CAP);
    running.await.unwrap();
}

struct MixedHealthProvider;

#[async_trait]
impl UsageProvider for MixedHealthProvider {
    fn name(&self) -> &str {
        "mixed"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("healthy"), handle("degraded")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.stable_id() == "healthy" {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::failure(
                observed(Some("B")),
                None,
                FetchError::NoSession("logged out".into()),
            )
        }
    }
}

#[tokio::test]
async fn one_healthy_handle_prevents_provider_wide_degradation() {
    let registry = Registry::new(vec![Box::new(MixedHealthProvider)]);
    tick(&registry).await;

    let health = registry.health();
    assert_eq!(health.fresh, 1);
    assert!(health.degraded.is_empty());
}

/// A provider counted as healthy can still be publishing a dead account.
///
/// The health buckets count providers and take the best account; the array
/// carries one entry per account. So a consumer reading a provider out of the
/// `fresh` bucket and concluding its accounts are all usable skips a failed one
/// silently -- the failure is in the array the whole time, and health is not the
/// place to look for it.
///
/// Separate from the aggregation test above even though the setup is identical.
/// That test is named for the health rule; this asserts the consequence a
/// consumer acts on, and neither should be able to delete the other's coverage.
///
/// The state cannot be observed from this host's live output -- both
/// multi-account providers here are uniformly healthy -- so the disagreement has
/// to be constructed.
#[tokio::test]
async fn a_provider_in_the_fresh_bucket_can_still_publish_a_failed_account() {
    let registry = Registry::new(vec![Box::new(MixedHealthProvider)]);
    tick(&registry).await;

    // The health axis says this provider is fine.
    let health = registry.health();
    assert_eq!(health.fresh, 1);
    assert!(health.degraded.is_empty());

    // The array says one of its two accounts is not.
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 2, "one entry per account: {usage:?}");

    let healthy = usage
        .iter()
        .find(|entry| entry.account.as_deref() == Some("A"))
        .expect("the healthy account is published");
    assert!(healthy.usage.is_some());
    assert!(healthy.error.is_none());

    let failed = usage
        .iter()
        .find(|entry| entry.account.as_deref() == Some("B"))
        .expect("the failed account is published rather than hidden by its healthy sibling");
    assert!(failed.usage.is_none());
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|text| text.contains("logged out")),
        "the failed account states its own reason: {:?}",
        failed.error
    );
}

struct FlipProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for FlipProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::Upstream("503".into()))
        }
    }
}

/// A provider whose first fetch succeeds and whose later fetches return whatever
/// the real normalizer produces for an EMPTY response body. This drives the
/// classification through the actual refresher state machine rather than
/// asserting `classify()`'s return value, which would pass trivially.
struct EmptyBodyProvider {
    name: &'static str,
    calls: Mutex<usize>,
    empty_body_error: fn() -> FetchError,
}

#[async_trait]
impl UsageProvider for EmptyBodyProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            let usage = Usage {
                primary: Some(RateWindow {
                    used_percent: 42.0,
                    raw_used_percent: None,
                    resets_at: Some("2026-08-01T00:00:00Z".to_string()),
                    window_minutes: Some(300),
                    used_count: None,
                    total_count: None,
                }),
                ..Usage::default()
            };
            FetchAttempt::success(None, "test", usage)
        } else {
            FetchAttempt::failure(None, None, (self.empty_body_error)())
        }
    }
}

/// An empty response body must not discard a known-good window.
///
/// This asserts the OBSERVABLE stale-serving behavior through a real refresh
/// cycle: after a healthy fetch, a fetch whose body is empty must keep serving
/// the last-healthy 42% window (status stale) instead of replacing it with a
/// degraded entry. Reverting either normalizer's empty-body branch to
/// `FetchError::Decode` makes this fail, because `classify` routes Decode to
/// NonTransient, which replaces the entry and drops the window.
#[tokio::test]
async fn empty_response_body_keeps_serving_the_last_healthy_window() {
    for (name, empty_body_error) in [
        (
            "alibaba",
            (|| crate::alibaba::normalize_usage(b"").expect_err("empty body must error"))
                as fn() -> FetchError,
        ),
        ("qwen-cloud", || {
            crate::qwen_cloud::normalize_usage(br#"{"successResponse":true}"#)
                .expect_err("empty envelope must error")
        }),
        ("grok", || {
            crate::grok::normalize_usage(b"").expect_err("empty frames must error")
        }),
    ] {
        let registry = Registry::new(vec![Box::new(EmptyBodyProvider {
            name,
            calls: Mutex::new(0),
            empty_body_error,
        })]);
        tick(&registry).await;

        let healthy = registry.get_usage(None).await;
        assert_eq!(healthy.len(), 1, "{name}: expected one entry after success");
        assert_eq!(
            healthy[0]
                .usage
                .as_ref()
                .and_then(|usage| usage.primary.as_ref())
                .map(|window| window.used_percent),
            Some(42.0),
            "{name}: first fetch should serve the real window"
        );

        force_due(&registry, name);
        tick(&registry).await;

        let after_empty = registry.get_usage(None).await;
        assert_eq!(after_empty.len(), 1, "{name}: expected one entry");
        assert!(
            after_empty[0].error.is_none(),
            "{name}: an empty body must not produce a degraded entry, got {:?}",
            after_empty[0].error
        );
        assert_eq!(
            after_empty[0]
                .usage
                .as_ref()
                .and_then(|usage| usage.primary.as_ref())
                .map(|window| window.used_percent),
            Some(42.0),
            "{name}: the last-healthy window must still be served"
        );
        assert_eq!(
            registry.health().stale,
            1,
            "{name}: the slot should be stale-serving, not degraded"
        );
    }
}

/// The boundary that makes the rule above safe: a parse failure on a NON-EMPTY
/// body stays non-transient and DOES degrade, because real bytes that will not
/// parse may be a permanent contract break.
#[tokio::test]
async fn malformed_non_empty_body_still_degrades() {
    let registry = Registry::new(vec![Box::new(EmptyBodyProvider {
        name: "alibaba",
        calls: Mutex::new(0),
        empty_body_error: || {
            crate::alibaba::normalize_usage(b"{not json").expect_err("garbage must error")
        },
    })]);
    tick(&registry).await;
    force_due(&registry, "alibaba");
    tick(&registry).await;

    let after_garbage = registry.get_usage(None).await;
    assert_eq!(after_garbage.len(), 1);
    assert!(
        after_garbage[0].error.is_some(),
        "a malformed non-empty body must degrade, not stale-serve"
    );
    assert_eq!(registry.health().stale, 0);
}

#[tokio::test]
async fn transient_failure_keeps_serving_last_good_window() {
    let registry = Registry::new(vec![Box::new(FlipProvider {
        calls: Mutex::new(0),
    })]);
    tick(&registry).await;
    force_due(&registry, "codex");
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].error.is_none());
    assert_eq!(registry.health().stale, 1);
}

#[test]
fn poisoned_store_reports_failing() {
    let registry = registry(&[("codex", false, true), ("cursor", true, true)]);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = registry.store.lock().unwrap();
        panic!("poison the slot store mutex");
    }));
    assert!(registry.health().is_failing());
}

#[test]
fn stalled_refresher_reports_degraded() {
    let registry = registry(&[("codex", false, true)]);
    registry
        .store
        .lock()
        .unwrap()
        .mark_tick(Instant::now() - STALL_HORIZON - Duration::from_secs(1));
    assert!(registry.health().is_degraded());
}

struct PanicProvider;

#[async_trait]
impl UsageProvider for PanicProvider {
    fn name(&self) -> &str {
        "boom"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        panic!("provider blew up mid-fetch");
    }
}

#[tokio::test]
async fn panicking_provider_is_contained_and_other_provider_resolves() {
    let registry = Registry::new(vec![
        Box::new(PanicProvider),
        Box::new(StubProvider {
            name: "codex",
            cookie: false,
            ok: true,
        }),
    ]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert!(usage
        .iter()
        .find(|entry| entry.provider == "boom")
        .expect("the panicking provider still produces an entry")
        .error
        .is_some());
    assert!(usage
        .iter()
        .find(|entry| entry.provider == "codex")
        .unwrap()
        .error
        .is_none());
}

/// A panic is this module's defect, and the published entry must say so.
///
/// Reporting it as a decode failure would state that the upstream sent a
/// payload we could not parse, sending anyone who acts on it to the provider's
/// API rather than to this codebase.
///
/// Separate from the containment test above even though the setup is
/// identical. That test is named for containment, so a later pass narrowing it
/// to its stated subject would delete this assertion without anything looking
/// wrong -- and an audit listing test names would report attribution as
/// untested while a mutation run reported it as covered.
#[tokio::test]
async fn a_contained_panic_is_attributed_to_this_module_not_the_upstream() {
    let registry = Registry::new(vec![Box::new(PanicProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    let text = usage[0]
        .error
        .as_deref()
        .expect("a contained panic still publishes an entry");
    assert!(
        text.starts_with("internal error:"),
        "panic attributed as {text:?}"
    );
    // Not vacuous: it is the panic being described, not some other failure that
    // happens to carry the same prefix.
    assert!(text.contains("panicked"), "unexpected: {text:?}");
}

#[tokio::test]
async fn cancelled_tick_leaves_slots_pending_without_hanging() {
    let registry = Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
    })]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    tokio::time::timeout(Duration::from_millis(200), registry.refresh_tick(&cancel))
        .await
        .expect("cancelled turn did not return promptly");
    assert!(registry.get_usage(None).await.is_empty());
    assert_eq!(
        slot(&registry, "codex", "implicit-local").status,
        SlotStatus::Pending
    );
}

struct OuterTimeoutSwapProvider {
    calls: AtomicUsize,
    current_account: Arc<Mutex<&'static str>>,
}

#[async_trait]
impl UsageProvider for OuterTimeoutSwapProvider {
    fn name(&self) -> &str {
        "outer-timeout"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            *self.current_account.lock().unwrap() = "B";
            std::future::pending::<()>().await;
            unreachable!("the scheduler deadline must terminate this attempt")
        }
    }
}

/// A fetch that outruns the scheduler deadline is reported as an upstream
/// failure, and that attribution is load-bearing rather than cosmetic: it is
/// the only transient class among the three synthetic failures, so a slow
/// provider keeps serving its last healthy window instead of losing it.
///
/// Calling this our own defect -- as a panic correctly is -- would make it
/// non-transient, and a provider that merely answered slowly would have its
/// cached window replaced by a degraded entry.
#[tokio::test]
async fn a_deadline_overrun_is_attributed_to_the_upstream_and_stays_transient() {
    let registry = Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
    })]);

    registry
        .refresh_tick_with_deadline(&CancellationToken::new(), Duration::from_millis(20))
        .await;

    let entry = &registry.get_usage(None).await[0];
    let text = entry
        .error
        .as_deref()
        .expect("the overrun degrades the slot");
    assert!(
        text.starts_with("upstream error:"),
        "deadline overrun attributed as {text:?}"
    );
    // Not vacuous: it is the overrun being described, not some other failure.
    assert!(text.contains("deadline"), "unexpected: {text:?}");

    // The consequence the attribution buys, asserted rather than implied.
    assert_eq!(
        crate::refresh::classify(&crate::provider::FetchError::Upstream(String::new())),
        crate::refresh::FetchClass::Transient,
    );
}

#[tokio::test]
async fn outer_timeout_after_credential_swap_fails_closed() {
    let current_account = Arc::new(Mutex::new("A"));
    let provider = OuterTimeoutSwapProvider {
        calls: AtomicUsize::new(0),
        current_account: Arc::clone(&current_account),
    };
    let registry = Registry::new(vec![Box::new(provider)]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("A")
    );

    force_due(&registry, "outer-timeout");
    registry
        .refresh_tick_with_deadline(&CancellationToken::new(), Duration::from_millis(20))
        .await;

    assert_eq!(*current_account.lock().unwrap(), "B");
    assert!(registry.get_usage(None).await.is_empty());
    let slot = slot(&registry, "outer-timeout", "implicit-local");
    assert!(slot.label_in_flux);
    assert!(slot.entry.is_none());
    assert!(slot.last_success_at.is_none());
    assert_eq!(slot.retry_count, 1);
}

struct PanickingNameProvider;

#[async_trait]
impl UsageProvider for PanickingNameProvider {
    fn name(&self) -> &str {
        panic!("provider name lookup panicked")
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(None, "test", Usage::default())
    }
}

#[tokio::test]
async fn panicking_provider_name_is_contained_at_registration() {
    let registry = Registry::new(vec![Box::new(PanickingNameProvider)]);
    assert_eq!(registry.provider_names(), vec!["invalid-provider-0"]);

    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].provider, "invalid-provider-0");
}

struct DropBomb;

impl Drop for DropBomb {
    fn drop(&mut self) {
        panic!("provider future panicked while being dropped")
    }
}

struct DropPanicProvider {
    started: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for DropPanicProvider {
    fn name(&self) -> &str {
        "drop-panic"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let _bomb = DropBomb;
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("cancellation must terminate this attempt")
    }
}

#[tokio::test]
async fn cancellation_contains_a_provider_future_drop_panic() {
    let started = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(DropPanicProvider {
        started: Arc::clone(&started),
    })]));
    let cancel = CancellationToken::new();
    let running = {
        let registry = Arc::clone(&registry);
        let cancel = cancel.clone();
        tokio::spawn(async move { registry.refresh_tick(&cancel).await })
    };
    started.notified().await;
    cancel.cancel();

    running
        .await
        .expect("provider teardown panic escaped the supervised task");
    assert_eq!(
        slot(&registry, "drop-panic", "implicit-local").status,
        SlotStatus::Pending
    );
}

struct CursorProvider {
    name: String,
    handles: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for CursorProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok((0..self.handles)
            .map(|index| handle(&format!("H{index}")))
            .collect())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        FetchAttempt::success(None, "test", Usage::default())
    }
}

/// Every registered provider lands in exactly one health bucket, including one
/// whose first fetch has not run yet.
///
/// Consumers are told to assert `fresh + stale + pending + degraded +
/// unconfigured + withoutHandles == providersTotal` and alert on an imbalance,
/// so a state that breaks it without anything being wrong is worse than no
/// invariant at all: it fires after every restart and trains the reader to
/// ignore the alert.
///
/// The unbucketed state is ordinary rather than exotic. The refresher admits a
/// bounded number of fetch units per turn, so any registry with more units than
/// that cap has providers still queued after the first tick -- which is when the
/// documented precondition (`lastTickAgeSecs` is set) says the identity holds.
#[tokio::test]
async fn every_provider_lands_in_exactly_one_health_bucket() {
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    // The providers before the cap take two handles each, so the cap is spent
    // before the last provider is reached and its slots stay Pending.
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: if index < refresh::CONCURRENCY_CAP {
                    2
                } else {
                    1
                },
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    tick(&registry).await;

    let health = registry.health();
    // The precondition consumers gate on: the identity is only claimed once the
    // refresher has ticked.
    assert!(health.last_tick_age.is_some());

    // Not vacuous: this is the state that breaks the identity when Pending has
    // no bucket. Without it the assertion below would pass on any all-fresh run.
    assert_eq!(
        calls[provider_count - 1].load(Ordering::SeqCst),
        0,
        "the last provider must not have been fetched yet"
    );
    assert_eq!(health.pending, 1, "the unfetched provider is pending");

    assert_eq!(
        health.fresh
            + health.stale
            + health.pending
            + health.degraded.len()
            + health.unconfigured.len()
            + health.without_handles.len(),
        health.providers_total,
        "buckets: fresh {} stale {} pending {} degraded {:?} unconfigured {:?} withoutHandles {:?} total {}",
        health.fresh,
        health.stale,
        health.pending,
        health.degraded,
        health.unconfigured,
        health.without_handles,
        health.providers_total
    );

    // And it still balances once everything has been fetched, so the fix is not
    // simply moving the imbalance to a later turn.
    tick(&registry).await;
    let health = registry.health();
    assert_eq!(health.pending, 0);
    assert_eq!(
        health.fresh
            + health.stale
            + health.pending
            + health.degraded.len()
            + health.unconfigured.len()
            + health.without_handles.len(),
        health.providers_total
    );
}

#[tokio::test]
async fn persisted_round_robin_cursor_admits_provider_after_the_first_cap() {
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: if index < refresh::CONCURRENCY_CAP {
                    2
                } else {
                    1
                },
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    tick(&registry).await;
    assert_eq!(calls[provider_count - 1].load(Ordering::SeqCst), 0);
    tick(&registry).await;

    assert_eq!(calls[provider_count - 1].load(Ordering::SeqCst), 1);
}

struct OverlappingAttemptProvider {
    calls: AtomicUsize,
    old_started: Arc<Notify>,
    old_gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for OverlappingAttemptProvider {
    fn name(&self) -> &str {
        "overlap"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.old_started.notify_one();
            self.old_gate.notified().await;
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::success(observed(Some("B")), "test", Usage::default())
        }
    }
}

#[tokio::test]
async fn older_overlapping_attempt_cannot_overwrite_newer_result() {
    let old_started = Arc::new(Notify::new());
    let old_gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(OverlappingAttemptProvider {
        calls: AtomicUsize::new(0),
        old_started: Arc::clone(&old_started),
        old_gate: Arc::clone(&old_gate),
    })]));

    let old_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    old_started.notified().await;
    let incarnation = slot(&registry, "overlap", "implicit-local").incarnation;

    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("B")
    );
    assert_eq!(
        slot(&registry, "overlap", "implicit-local").incarnation,
        incarnation
    );

    old_gate.notify_one();
    old_tick.await.unwrap();
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("B")
    );
}

#[test]
fn unobserved_success_cannot_close_label_flux() {
    let now = Instant::now();
    let cold = ProviderSlot::due_now(now, Incarnation::from_counter(1));
    let account_a = next_slot_after_attempt(
        &cold,
        "flux",
        FetchAttempt::success(observed(Some("A")), "test", Usage::default()),
        now,
        now,
    );
    let account_b_failed = next_slot_after_attempt(
        &account_a,
        "flux",
        FetchAttempt::failure(
            observed(Some("B")),
            Some("test".into()),
            FetchError::Upstream("timeout".into()),
        ),
        now,
        now,
    );
    assert!(account_b_failed.label_in_flux);

    let unobserved_success = next_slot_after_attempt(
        &account_b_failed,
        "flux",
        FetchAttempt::success(None, "test", Usage::default()),
        now,
        now,
    );
    assert!(unobserved_success.label_in_flux);
    assert!(unobserved_success.entry.is_none());

    let observed_success = next_slot_after_attempt(
        &unobserved_success,
        "flux",
        FetchAttempt::success(observed(Some("B")), "test", Usage::default()),
        now,
        now,
    );
    assert!(!observed_success.label_in_flux);
    assert_eq!(observed_success.account_id(), Some("B"));
}

struct DuplicateAccountProvider;

#[async_trait]
impl UsageProvider for DuplicateAccountProvider {
    fn name(&self) -> &str {
        "duplicate"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("A-degraded"), handle("B-fresh")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.stable_id() == "A-degraded" {
            FetchAttempt::failure(
                observed(Some("shared-account")),
                None,
                FetchError::NoSession("logged out".into()),
            )
        } else {
            FetchAttempt::success(observed(Some("shared-account")), "test", Usage::default())
        }
    }
}

#[tokio::test]
async fn duplicate_account_prefers_fresh_handle_over_earlier_degraded_handle() {
    let registry = Registry::new(vec![Box::new(DuplicateAccountProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("shared-account"));
    assert!(usage[0].error.is_none());
    assert!(usage[0].usage.is_some());
}

#[test]
fn adapter_distinguishes_absent_label_from_unavailable_observation() {
    let attempt = FetchAttempt::from_provider_usage(Ok(ProviderUsage::healthy(
        "unlabeled",
        None,
        "test",
        Usage::default(),
    )));
    assert!(attempt.observed.is_some());
    assert_eq!(attempt.observed.unwrap().account_id, None);
}

static RESET_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct ResetTempDir {
    dir: std::path::PathBuf,
}

impl ResetTempDir {
    fn new(label: &str) -> Self {
        let id = RESET_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ck-quota-reset-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn journal(&self) -> RedemptionJournal {
        RedemptionJournal::new(self.dir.join("redemptions.json"))
    }
}

impl Drop for ResetTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn reset_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0)
        .single()
        .unwrap()
}

fn reset_request(bearer: &str) -> ResetRequest {
    ResetRequest {
        base_url: "https://example.invalid/backend-api".to_string(),
        bearer: bearer.to_string(),
        account_id: "acct-reset".to_string(),
        auth_failure: None,
    }
}

fn reset_facts(percent: f64, at_wall: bool) -> UsageFacts {
    UsageFacts {
        raw_percents: vec![percent],
        any_used_floor: percent >= 1.0,
        at_wall,
        wall_clear: !at_wall,
    }
}

fn reset_tick_input(now: chrono::DateTime<Utc>, facts: UsageFacts) -> ResetTickInput {
    ResetTickInput {
        armed: true,
        now,
        earliest_expiry: Some(now + chrono::Duration::minutes(5)),
        auto_use_resets_secs: 10 * 60,
        facts,
        elapsed_since_attempt_start: Duration::from_secs(1),
    }
}

#[derive(Clone, Copy)]
enum MockConsumeBehavior {
    Outcome(ConsumeOutcome),
    Error,
    Status(u16),
    Hang,
    Block(ConsumeOutcome),
}

struct MockResetTransport {
    behavior: MockConsumeBehavior,
    gets: AtomicUsize,
    posts: AtomicUsize,
    consume_accounts: Mutex<Vec<String>>,
    reservation_visible: AtomicBool,
    journal_path: Option<std::path::PathBuf>,
    credits_error: bool,
    started: Semaphore,
    release: Semaphore,
}

impl MockResetTransport {
    fn new(behavior: MockConsumeBehavior) -> Self {
        Self {
            behavior,
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            consume_accounts: Mutex::new(Vec::new()),
            reservation_visible: AtomicBool::new(false),
            journal_path: None,
            credits_error: false,
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    fn with_journal(mut self, journal: &RedemptionJournal) -> Self {
        self.journal_path = Some(journal.path().to_path_buf());
        self
    }

    fn credits_failure(mut self) -> Self {
        self.credits_error = true;
        self
    }

    fn body(outcome: ConsumeOutcome) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "code": outcome.as_code(),
            "windows_reset": ["primary"]
        }))
        .unwrap()
    }
}

#[async_trait]
impl ResetTransport for MockResetTransport {
    async fn fetch_credits(
        &self,
        _request: &ResetRequest,
    ) -> Result<CreditsHttpResponse, FetchError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.credits_error {
            return Err(FetchError::Upstream("mock credits failure".into()));
        }
        Ok(CreditsHttpResponse {
            body: br#"{
                "credits": [{
                    "id": "credit-1",
                    "status": "available",
                    "expires_at": "2026-07-15T12:00:00Z"
                }],
                "available_count": 1
            }"#
            .to_vec(),
            date_header: Some("Tue, 14 Jul 2026 12:00:00 +0000".to_string()),
        })
    }

    async fn consume(
        &self,
        request: &ResetRequest,
        redeem_request_id: &str,
    ) -> Result<Vec<u8>, FetchError> {
        if let Some(path) = &self.journal_path {
            let records: Vec<crate::codex_resets::RedemptionRecord> =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            self.reservation_visible.store(
                records.iter().any(|record| {
                    record.redeem_request_id == redeem_request_id
                        && record.status == crate::codex_resets::JournalStatus::Pending
                        && record.last_attempt_at.is_some()
                        && record.attempt_count == 1
                }),
                Ordering::SeqCst,
            );
        }
        self.posts.fetch_add(1, Ordering::SeqCst);
        self.consume_accounts
            .lock()
            .unwrap()
            .push(request.account_id.clone());
        self.started.add_permits(1);
        match self.behavior {
            MockConsumeBehavior::Outcome(outcome) => Ok(Self::body(outcome)),
            MockConsumeBehavior::Error => Err(FetchError::Upstream("mock HTTP 503".into())),
            MockConsumeBehavior::Status(status) => Err(FetchError::ProviderStatus(status)),
            MockConsumeBehavior::Hang => {
                std::future::pending::<Result<Vec<u8>, FetchError>>().await
            }
            MockConsumeBehavior::Block(outcome) => {
                let permit = self.release.acquire().await.unwrap();
                permit.forget();
                Ok(Self::body(outcome))
            }
        }
    }
}

#[derive(Default)]
struct MockReportingCredentialSource {
    reports: Mutex<Vec<(u16, u64)>>,
}

#[async_trait]
impl CredentialSource for MockReportingCredentialSource {
    async fn get(
        &self,
        _capability: &VaultCapability,
        _min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        Err(VaultGetError::Permanent)
    }

    async fn report_auth_failure(
        &self,
        _capability: &VaultCapability,
        provider_status: u16,
        record_version: u64,
    ) {
        self.reports
            .lock()
            .unwrap()
            .push((provider_status, record_version));
    }
}

fn reporting_reset_request(
    source: Arc<MockReportingCredentialSource>,
    secret: &str,
) -> ResetRequest {
    let source: Arc<dyn CredentialSource> = source;
    ResetRequest {
        base_url: "https://example.invalid/backend-api".to_string(),
        bearer: secret.to_string(),
        account_id: "acct-reset".to_string(),
        auth_failure: Some(AuthFailureContext {
            source,
            capability: VaultCapability::new("ckh_reporting_secret"),
            record_version: 31,
        }),
    }
}

/// A rejected vault credential must reach the credential store as a reportable
/// failure, and that depends on which error the HTTP layer produces.
///
/// The reporting gate matches only `ProviderStatus`, so a 401 mapped to
/// `Unauthorized` is dropped silently: the store is never told the credential
/// is dead, no re-login is prompted, and the account stays dark until someone
/// investigates by hand. Which mapping happens is decided by a dispatch inside
/// the transport, and every other test in this area hands the gate a
/// pre-built error, so none of them exercise that decision.
///
/// This drives a real request at a loopback server answering 401.
#[tokio::test]
async fn a_rejected_vault_credential_surfaces_as_a_reportable_status() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 8 * 1024];
        let _ = stream.read(&mut buffer).await.unwrap();
        let body = r#"{"error":"invalid_token"}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let source = Arc::new(MockReportingCredentialSource::default());
    let request = ResetRequest {
        base_url: format!("http://{address}/backend-api"),
        ..reporting_reset_request(Arc::clone(&source), "reset-bearer-secret")
    };

    let transport = ReqwestResetTransport::new(reqwest::Client::new());
    let error = transport
        .fetch_credits(&request)
        .await
        .expect_err("a 401 must fail");

    // The gate's own condition, asserted against what the transport actually
    // produced rather than against a value constructed here.
    assert!(
        matches!(error, FetchError::ProviderStatus(401)),
        "a rejected credential produced {error:?}, which the reporting gate drops"
    );

    // And the consequence: the store is told, with the version that was served.
    request.report_auth_failure(&error);
    tokio::task::yield_now().await;
    assert_eq!(*source.reports.lock().unwrap(), vec![(401, 31)]);

    server.await.unwrap();
}

#[tokio::test]
async fn credits_and_consume_auth_failures_report_served_version() {
    let source = Arc::new(MockReportingCredentialSource::default());
    let request = reporting_reset_request(Arc::clone(&source), "reset-bearer-secret");

    request.report_auth_failure(&FetchError::ProviderStatus(401));
    tokio::task::yield_now().await;

    let temp = ResetTempDir::new("consume-auth-report");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Status(401));
    coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &transport,
            &request,
        )
        .await;
    tokio::task::yield_now().await;

    let mut reports = source.reports.lock().unwrap().clone();
    reports.sort_unstable();
    assert_eq!(reports, vec![(401, 31), (401, 31)]);
}

#[test]
fn reset_request_debug_redacts_bearer_and_capability() {
    let source = Arc::new(MockReportingCredentialSource::default());
    let request = reporting_reset_request(source, "reset-debug-secret");
    let debug = format!("{request:?}");
    assert!(!debug.contains("reset-debug-secret"));
    assert!(!debug.contains("ckh_reporting_secret"));
    assert!(debug.contains("redacted"));
}

struct SameAccountVaultSource {
    gets: AtomicUsize,
}

#[async_trait]
impl CredentialSource for SameAccountVaultSource {
    async fn get(
        &self,
        _capability: &VaultCapability,
        min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        assert_eq!(min_ttl_ms, 120_000);
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(VaultCredential {
            payload: b"real-provider-vault-token".to_vec(),
            expires_at_ms: None,
            record_version: 8,
            account_id: Some("real-provider-account".to_string()),
            email: None,
            org_name: None,
            project_id: None,
        })
    }

    async fn report_auth_failure(
        &self,
        _capability: &VaultCapability,
        _provider_status: u16,
        _record_version: u64,
    ) {
    }
}

fn write_owner_only_test_file(path: &std::path::Path, body: &[u8]) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).unwrap();
    std::io::Write::write_all(&mut file, body).unwrap();
}

#[tokio::test]
async fn i9_real_codex_provider_two_units_same_account_send_one_consume_post() {
    let temp = ResetTempDir::new("real-provider-two-unit-one-post");
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let http_server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.lines().any(|line| {
                line.eq_ignore_ascii_case("chatgpt-account-id: real-provider-account")
            }));
            assert!(request.lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer real-provider-local-token")
                    || line.eq_ignore_ascii_case("authorization: Bearer real-provider-vault-token")
            }));
            let body = serde_json::json!({
                "rate_limit": {
                    "limit_reached": true,
                    "primary_window": {
                        "used_percent": 100.0,
                        "reset_at": 1_900_000_000_i64,
                        "limit_window_seconds": 604_800
                    }
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let codex_home = temp.dir.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    write_owner_only_test_file(
        &codex_home.join("auth.json"),
        br#"{"tokens":{"access_token":"real-provider-local-token","account_id":"real-provider-account"}}"#,
    );
    write_owner_only_test_file(
        &codex_home.join("config.toml"),
        format!(
            "chatgpt_base_url = {:?}\n",
            format!("http://{address}/backend-api")
        )
        .as_bytes(),
    );
    let handles_path = temp.dir.join("vault-handles.json");
    write_owner_only_test_file(
        &handles_path,
        br#"{"handles":{"chatgpt:openai":"ckh_real_provider"}}"#,
    );

    let source = Arc::new(SameAccountVaultSource {
        gets: AtomicUsize::new(0),
    });
    let credential_source: Arc<dyn CredentialSource> = source.clone();
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::Reset,
    )));
    let reset_transport: Arc<dyn ResetTransport> = transport.clone();
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let provider = crate::codex::CodexProvider::new_for_test(
        crate::config::CodexConfig {
            auto_use_resets: 172_800,
        },
        Some(credential_source),
        reset_transport,
        coordinator,
        VaultHandleLoader::new(Some(handles_path)),
        codex_home,
    );
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    http_server.await.unwrap();

    assert_eq!(source.gets.load(Ordering::SeqCst), 1);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *transport.consume_accounts.lock().unwrap(),
        vec!["real-provider-account".to_string()]
    );
    let usage = registry.get_usage(Some("codex")).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("real-provider-account"));
}

struct TwoUnitResetProvider {
    coordinator: Arc<ResetCoordinator>,
    transport: Arc<MockResetTransport>,
}

#[async_trait]
impl UsageProvider for TwoUnitResetProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![
            CredentialHandle::implicit(),
            CredentialHandle::vault(
                "chatgpt:openai:gmail",
                crate::credential_source::VaultCapability::new("ckh_same_account"),
            ),
        ])
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let facts = reset_facts(100.0, true);
        self.coordinator
            .process_tick(
                "same-codex-account",
                reset_tick_input(reset_now(), facts),
                self.transport.as_ref(),
                &ResetRequest {
                    base_url: "https://example.invalid/backend-api".to_string(),
                    bearer: "same-account-token".to_string(),
                    account_id: "same-codex-account".to_string(),
                    auth_failure: None,
                },
            )
            .await;
        FetchAttempt::success(
            observed(Some("same-codex-account")),
            "test",
            full_usage(100.0),
        )
    }
}

#[tokio::test]
async fn registry_scheduler_two_codex_units_same_account_send_one_consume_post() {
    let temp = ResetTempDir::new("registry-two-unit-one-post");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::Reset,
    )));
    let registry = Registry::new(vec![Box::new(TwoUnitResetProvider {
        coordinator,
        transport: Arc::clone(&transport),
    })]);

    tick(&registry).await;

    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    let usage = registry.get_usage(Some("codex")).await;
    assert_eq!(usage.len(), 1, "same-account slots must deduplicate");
    assert_eq!(usage[0].account.as_deref(), Some("same-codex-account"));
}

#[test]
fn codex_reset_trigger_truth_table_is_fully_fenced() {
    let now = reset_now();
    let base = TriggerInput {
        armed: true,
        now,
        earliest_expiry: Some(now + chrono::Duration::minutes(5)),
        auto_use_resets_secs: 10 * 60,
        any_used_floor: true,
        at_wall: false,
        pending: false,
        spend_bound_allows: true,
        before_post_cutoff: true,
    };
    let mut cases = Vec::new();
    cases.push(("expiry", base.clone(), true));

    let mut unarmed = base.clone();
    unarmed.armed = false;
    cases.push(("unarmed", unarmed, false));

    let mut outside_expiry = base.clone();
    outside_expiry.earliest_expiry = Some(now + chrono::Duration::hours(1));
    cases.push(("outside-expiry", outside_expiry, false));

    let mut no_floor = base.clone();
    no_floor.any_used_floor = false;
    cases.push(("no-used-floor", no_floor, false));

    let mut exhaustion = base.clone();
    exhaustion.any_used_floor = false;
    exhaustion.earliest_expiry = None;
    exhaustion.at_wall = true;
    cases.push(("exhaustion", exhaustion, true));

    let mut pending = base.clone();
    pending.pending = true;
    cases.push(("pending", pending, false));

    let mut spend_bound = base.clone();
    spend_bound.spend_bound_allows = false;
    cases.push(("spend-bound", spend_bound, false));

    let mut cutoff = base;
    cutoff.before_post_cutoff = false;
    cases.push(("pre-post-cutoff", cutoff, false));

    for (name, input, expected) in cases {
        assert_eq!(evaluate_trigger(&input).fire, expected, "case {name}");
    }
}

#[test]
fn redemption_journal_reuses_pending_id_after_restart() {
    let temp = ResetTempDir::new("restart");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal.reserve("acct", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected a new reservation, got {other:?}"),
    };

    let parsed_id = uuid::Uuid::parse_str(&id).expect("journal id is a UUID");
    assert_eq!(parsed_id.get_version_num(), 4);

    let restarted = RedemptionJournal::new(journal.path().to_path_buf());
    assert_eq!(
        restarted.reserve("acct", now + chrono::Duration::minutes(1)),
        Ok(Reservation::ExistingPending(id.clone()))
    );
    let records = restarted.records().unwrap();
    assert_eq!(records.len(), 1, "restart must not mint a second id");
    assert_eq!(records[0].redeem_request_id, id);
}

#[tokio::test]
async fn restarted_coordinator_reposts_the_same_pending_id_without_a_new_trigger() {
    let temp = ResetTempDir::new("restart-repost");
    let now = reset_now();
    let journal = temp.journal();
    let pending_id = match journal.reserve("acct-reset", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected reservation, got {other:?}"),
    };
    let restarted =
        ResetCoordinator::new(RedemptionJournal::new(journal.path().to_path_buf())).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::AlreadyRedeemed,
    ));
    let mut input = reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(0.0, false));
    input.earliest_expiry = Some(now + chrono::Duration::days(1));

    let result = restarted
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;

    assert!(result.consume_attempted, "pending id was not retried");
    assert_eq!(result.outcome, Some(ConsumeOutcome::AlreadyRedeemed));
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "retry minted a second logical redemption");
    assert_eq!(records[0].redeem_request_id, pending_id);
    assert_eq!(records[0].outcome, Some(ConsumeOutcome::AlreadyRedeemed));
}

#[test]
fn f2_old_pending_redemption_is_reused_forever() {
    let temp = ResetTempDir::new("old-pending");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal
        .reserve("acct", now - chrono::Duration::hours(25))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected a new pending id, got {other:?}"),
    };

    assert_eq!(
        journal.reserve("acct", now),
        Ok(Reservation::ExistingPending(id.clone()))
    );
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "old pending id was replaced");
    assert_eq!(records[0].redeem_request_id, id);
    assert_eq!(
        records[0].status,
        crate::codex_resets::JournalStatus::Pending
    );
    assert_eq!(records[0].outcome, None);
}

#[test]
fn f2_late_pending_attempt_starts_durable_spend_bound() {
    let temp = ResetTempDir::new("late-pending-bound");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal
        .reserve("acct", now - chrono::Duration::hours(1))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected a new pending id, got {other:?}"),
    };
    journal.record_attempt("acct", &id, now).unwrap();
    journal.resolve("acct", &id, ConsumeOutcome::Reset).unwrap();

    let restarted = temp.journal();
    let bounded = restarted
        .inspect_account("acct", now + chrono::Duration::minutes(1))
        .unwrap();
    assert!(!bounded.spend_bound_allows);
    let records = restarted.records().unwrap();
    assert_eq!(records[0].last_attempt_at, Some(now.to_rfc3339()));
    assert_eq!(records[0].attempt_count, 1);
    assert_eq!(
        restarted
            .reserve("acct", now + chrono::Duration::minutes(1))
            .unwrap(),
        Reservation::SpendBound
    );
}

#[test]
fn f2_legacy_journal_records_default_attempt_metadata() {
    let temp = ResetTempDir::new("legacy-attempt-fields");
    let journal = temp.journal();
    std::fs::write(
        journal.path(),
        format!(
            r#"[{{"account_id":"acct","redeem_request_id":"legacy-id","created_at":"{}","status":"pending","outcome":null}}]"#,
            reset_now().to_rfc3339()
        ),
    )
    .unwrap();

    let records = journal.records().unwrap();
    assert_eq!(records[0].last_attempt_at, None);
    assert_eq!(records[0].attempt_count, 0);
}

/// Retention removes settled records and only settled ones.
///
/// Driven through `inspect_account`, which is the only door production opens:
/// pruning happens as a side effect of asking about an account, so a test
/// calling a prune entry point directly would exercise a path the module never
/// takes and could pass while the production one did nothing.
#[test]
fn redemption_journal_prunes_old_resolved_records_only() {
    let temp = ResetTempDir::new("prune");
    let now = reset_now();
    let journal = temp.journal();
    let old_id = match journal
        .reserve("old", now - chrono::Duration::days(8))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected old reservation, got {other:?}"),
    };
    journal
        .resolve("old", &old_id, ConsumeOutcome::Reset)
        .unwrap();
    let recent_id = match journal
        .reserve("recent", now - chrono::Duration::days(1))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected recent reservation, got {other:?}"),
    };
    journal
        .resolve("recent", &recent_id, ConsumeOutcome::NoCredit)
        .unwrap();

    journal.inspect_account("old", now).unwrap();

    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].account_id, "recent");
}

/// A pending record is never pruned, however old it is.
///
/// This is the fence against spending a banked credit twice. A pending record
/// is a redemption whose outcome the server never confirmed, and its id is the
/// only thing tying a retry to the original attempt. If retention removed it,
/// the next reservation would mint a FRESH id and post the same logical
/// redemption a second time -- which spends a real credit on a real account,
/// and is not recoverable.
///
/// The age here is deliberately past the retention horizon that governs settled
/// records, expressed relative to the horizon rather than as a fixed number of
/// days, so shortening that constant cannot leave this case inside the window
/// and silently stop testing anything.
#[test]
fn a_pending_redemption_is_never_pruned_however_old() {
    let temp = ResetTempDir::new("pending-outlives-retention");
    let now = reset_now();
    let journal = temp.journal();

    let horizon = chrono::Duration::seconds(crate::codex_resets::RESOLVED_RETENTION_SECS);
    let reserved_at = now - horizon - chrono::Duration::days(1);
    let pending_id = match journal.reserve("acct", reserved_at).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected a new pending id, got {other:?}"),
    };

    // Asking about an account is what triggers retention in production, so this
    // is the call that would drop the record if the status guard were wrong.
    let state = journal.inspect_account("acct", now).unwrap();
    assert_eq!(
        state.pending_id.as_deref(),
        Some(pending_id.as_str()),
        "an unconfirmed redemption was pruned; a retry would mint a second id \
         for the same redemption and spend the credit twice"
    );

    // And the id a caller would actually reuse is still that one.
    assert_eq!(
        journal.reserve("acct", now),
        Ok(Reservation::ExistingPending(pending_id.clone()))
    );
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].redeem_request_id, pending_id);
}

#[test]
fn corrupt_redemption_journal_fails_closed() {
    let temp = ResetTempDir::new("corrupt");
    let journal = temp.journal();
    std::fs::write(journal.path(), b"not json").unwrap();
    assert!(journal.records().is_err());
    assert!(journal.inspect_account("acct", reset_now()).is_err());
}

#[test]
fn spend_bound_survives_a_journal_restart() {
    let temp = ResetTempDir::new("spend-bound");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal.reserve("acct", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected reservation, got {other:?}"),
    };
    journal
        .resolve("acct", &id, ConsumeOutcome::NothingToReset)
        .unwrap();

    let restarted = RedemptionJournal::new(journal.path().to_path_buf());
    assert_eq!(
        restarted.reserve("acct", now + chrono::Duration::minutes(29)),
        Ok(Reservation::SpendBound)
    );
    assert!(matches!(
        restarted
            .reserve("acct", now + chrono::Duration::minutes(31))
            .unwrap(),
        Reservation::New(_)
    ));
}

#[tokio::test]
async fn pre_post_cutoff_prevents_reservation_and_mutation() {
    let temp = ResetTempDir::new("pre-post-cutoff");
    let journal = temp.journal();
    let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let mut input = reset_tick_input(reset_now(), reset_facts(100.0, true));
    input.elapsed_since_attempt_start = crate::codex_resets::PRE_POST_CUTOFF;

    let result = coordinator
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;

    assert!(!result.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    assert!(journal.records().unwrap().is_empty());
}

#[tokio::test]
async fn f2_redemption_and_attempt_metadata_are_durable_before_post() {
    let temp = ResetTempDir::new("reserve-before-post");
    let journal = temp.journal();
    let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset))
        .with_journal(&journal);
    let now = reset_now();

    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-a"),
        )
        .await;

    assert!(result.consume_attempted);
    assert!(transport.reservation_visible.load(Ordering::SeqCst));
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

async fn wait_for_mock_post(transport: &MockResetTransport) {
    let permit = transport.started.acquire().await.unwrap();
    permit.forget();
}

#[tokio::test]
async fn overlapping_fetches_for_one_account_send_exactly_one_post() {
    let temp = ResetTempDir::new("overlap-one-account");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Block(
        ConsumeOutcome::Reset,
    )));
    let now = reset_now();

    let first = {
        let coordinator = Arc::clone(&coordinator);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            coordinator
                .process_tick(
                    "shared-account",
                    reset_tick_input(now, reset_facts(100.0, true)),
                    transport.as_ref(),
                    &reset_request("same-handle"),
                )
                .await
        })
    };
    wait_for_mock_post(&transport).await;
    let second = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            transport.as_ref(),
            &reset_request("same-handle"),
        )
        .await;

    assert!(!second.consume_attempted);
    assert!(second.pending);
    assert!(!second.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    transport.release.add_permits(1);
    assert!(first.await.unwrap().consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unresolved_post_is_not_retried_again_in_the_same_tick() {
    let temp = ResetTempDir::new("same-tick-pending");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Error);
    let now = reset_now();

    let first = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-a"),
        )
        .await;
    let second = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-b"),
        )
        .await;

    assert!(first.consume_attempted);
    assert!(!second.consume_attempted);
    assert!(second.pending);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn two_handles_resolving_same_account_send_exactly_one_post() {
    let temp = ResetTempDir::new("two-handles-one-account");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Block(
        ConsumeOutcome::AlreadyRedeemed,
    )));
    let now = reset_now();

    let handle_a = {
        let coordinator = Arc::clone(&coordinator);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            coordinator
                .process_tick(
                    "same-account-id",
                    reset_tick_input(now, reset_facts(99.0, true)),
                    transport.as_ref(),
                    &reset_request("handle-a-token"),
                )
                .await
        })
    };
    wait_for_mock_post(&transport).await;
    let handle_b = coordinator
        .process_tick(
            "same-account-id",
            reset_tick_input(now, reset_facts(99.0, true)),
            transport.as_ref(),
            &reset_request("handle-b-token"),
        )
        .await;

    assert!(!handle_b.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    transport.release.add_permits(1);
    handle_a.await.unwrap();
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

fn primary_percent(entry: &ProviderUsage) -> f64 {
    entry
        .usage
        .as_ref()
        .and_then(|usage| usage.primary.as_ref())
        .expect("test usage has a primary window")
        .used_percent
}

fn full_usage(percent: f64) -> Usage {
    let window = |used_percent: f64, label: &str, minutes| RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: Some(format!("2026-07-15T{label}:00Z")),
        window_minutes: Some(minutes),
        used_count: None,
        total_count: None,
    };
    Usage {
        primary: Some(window(percent, "01:00", 300)),
        secondary: Some(window(percent + 1.0, "02:00", 10_080)),
        tertiary: Some(window(percent + 2.0, "03:00", 43_200)),
        extra_rate_windows: Some(vec![ExtraWindow {
            title: Some("model pool".to_string()),
            id: Some("model-1".to_string()),
            window: Some(window(percent + 3.0, "04:00", 60)),
        }]),
    }
}

struct RelaxingProvider {
    name: &'static str,
    eligible: bool,
    usage: Usage,
}

#[async_trait]
impl UsageProvider for RelaxingProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(None, "test", self.usage.clone()).with_relax_eligible(self.eligible)
    }
}

struct LabeledRelaxingProvider;

#[async_trait]
impl UsageProvider for LabeledRelaxingProvider {
    fn name(&self) -> &str {
        "codex-labeled"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(observed(Some("codex-account")), "test", full_usage(38.0))
            .with_relax_eligible(true)
    }
}

#[tokio::test]
async fn f8_labeled_emission_applies_fresh_relaxation_transform() {
    let registry = Registry::new(vec![Box::new(LabeledRelaxingProvider)]);
    tick(&registry).await;

    let served = registry.get_usage(Some("codex-labeled")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("codex-account"));
    assert_eq!(primary_percent(&served[0]), 0.0);
    assert_eq!(
        slot(&registry, "codex-labeled", "implicit-local")
            .entry
            .as_ref()
            .and_then(|entry| entry.usage.as_ref())
            .and_then(|usage| usage.primary.as_ref())
            .unwrap()
            .used_percent,
        38.0
    );
}

#[tokio::test]
async fn relaxation_is_read_time_only_and_expires_at_the_freshness_horizon() {
    let registry = Registry::new(vec![Box::new(RelaxingProvider {
        name: "codex",
        eligible: true,
        usage: full_usage(41.0),
    })]);
    tick(&registry).await;

    let stored = slot(&registry, "codex", "implicit-local");
    let stored_usage = stored.entry.as_ref().unwrap().usage.as_ref().unwrap();
    assert_eq!(stored_usage.primary.as_ref().unwrap().used_percent, 41.0);
    assert_eq!(stored_usage.secondary.as_ref().unwrap().used_percent, 42.0);
    assert_eq!(
        stored_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        44.0,
        "the slot must retain raw truth"
    );

    let fresh = registry.get_usage(None).await;
    let fresh_usage = fresh[0].usage.as_ref().unwrap();
    assert_eq!(fresh_usage.primary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(fresh_usage.secondary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(fresh_usage.tertiary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(
        fresh_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        0.0
    );
    // The provider-reported truth rides beside the effective zero so human
    // UIs can display real usage.
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().raw_used_percent,
        Some(41.0)
    );
    assert_eq!(
        fresh_usage.secondary.as_ref().unwrap().raw_used_percent,
        Some(42.0)
    );
    assert_eq!(
        fresh_usage.tertiary.as_ref().unwrap().raw_used_percent,
        Some(43.0)
    );
    assert_eq!(
        fresh_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .raw_used_percent,
        Some(44.0)
    );
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().resets_at,
        stored_usage.primary.as_ref().unwrap().resets_at
    );
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().window_minutes,
        Some(300)
    );

    let key = SlotKey::new("codex", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut stale = store.get(&key).unwrap().clone();
        stale.last_success_at = Some(Instant::now() - FRESH_HORIZON - Duration::from_secs(1));
        let incarnation = stale.incarnation;
        let sequence = stale.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, sequence, stale));
    }
    let stale = registry.get_usage(None).await;
    let stale_usage = stale[0].usage.as_ref().unwrap();
    assert_eq!(stale_usage.primary.as_ref().unwrap().used_percent, 41.0);
    assert_eq!(
        stale_usage.primary.as_ref().unwrap().raw_used_percent,
        None,
        "an unrelaxed window must not carry a raw annotation"
    );
    assert_eq!(stale_usage.secondary.as_ref().unwrap().used_percent, 42.0);
    assert_eq!(
        stale_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        44.0,
        "a relaxed percentage survived beyond freshness"
    );
}

struct DegradingRelaxProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl UsageProvider for DegradingRelaxProvider {
    fn name(&self) -> &str {
        "codex-degraded"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            FetchAttempt::success(None, "test", full_usage(55.0)).with_relax_eligible(true)
        } else {
            FetchAttempt::failure(None, None, FetchError::NoSession("signed out".to_string()))
        }
    }
}

struct TransientlyFailingRelaxProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl UsageProvider for TransientlyFailingRelaxProvider {
    fn name(&self) -> &str {
        "codex-stale-transient"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            FetchAttempt::success(None, "test", full_usage(62.0)).with_relax_eligible(true)
        } else {
            FetchAttempt::failure(
                None,
                Some("test".to_string()),
                FetchError::Upstream("temporary credits outage".to_string()),
            )
        }
    }
}

#[tokio::test]
async fn timestamp_formatting_runs_after_store_unlock() {
    let registry = Arc::new(registry(&[("format-lock", false, true)]));
    tick(&registry).await;
    let hook_ran = Arc::new(AtomicBool::new(false));

    BEFORE_FETCHED_AT_FORMAT.with(|hook| {
        let registry = Arc::clone(&registry);
        let hook_ran = Arc::clone(&hook_ran);
        *hook.borrow_mut() = Some(Box::new(move || {
            assert!(
                registry.store.try_lock().is_ok(),
                "store lock held during fetched_at formatting"
            );
            hook_ran.store(true, Ordering::SeqCst);
        }));
    });

    let usage = registry.get_usage(Some("format-lock")).await;
    assert_eq!(usage.len(), 1);
    assert!(hook_ran.load(Ordering::SeqCst));
    BEFORE_FETCHED_AT_FORMAT.with(|hook| *hook.borrow_mut() = None);
}

#[tokio::test]
async fn fetched_at_is_stable_for_fresh_and_stale_slot_entries() {
    let registry = Registry::new(vec![Box::new(TransientlyFailingRelaxProvider {
        calls: AtomicUsize::new(0),
    })]);
    tick(&registry).await;

    let fresh = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(fresh.len(), 1);
    let fetched_at = fresh[0].fetched_at.clone().expect("fresh slot timestamp");
    let parsed = chrono::DateTime::parse_from_rfc3339(&fetched_at).unwrap();
    assert!(
        (Utc::now() - parsed.with_timezone(&Utc))
            .num_seconds()
            .abs()
            < 5
    );

    force_due(&registry, "codex-stale-transient");
    tick(&registry).await;
    let stale = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(stale[0].fetched_at.as_deref(), Some(fetched_at.as_str()));
}

/// A provider whose local source is not running keeps serving its last window.
///
/// Some providers read usage from a program on this machine rather than from a
/// stored credential. That program's absence tracks whether someone has an
/// application open, so it comes and goes on a cadence no consumer can see --
/// unlike an absent credential, which is a stable fact about the host and which
/// a consumer may reasonably read as "this provider does not exist here" and
/// act on by discarding what it knows.
///
/// Classifying the two the same way makes such a provider appear and disappear
/// as windows are opened and closed. Treating it as transient keeps the last
/// known window in place across the gap, which is both more useful and more
/// honest: the capacity did not change, only our ability to read it.
/// Succeeds once, then fails with whatever the test asks for.
struct SucceedsThenFailsProvider {
    calls: Mutex<usize>,
    then: fn() -> FetchError,
}

#[async_trait]
impl UsageProvider for SucceedsThenFailsProvider {
    fn name(&self) -> &str {
        "local-lane"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, (self.then)())
        }
    }
}

#[tokio::test]
async fn a_provider_whose_local_source_stops_running_keeps_serving_its_window() {
    let registry = Registry::new(vec![Box::new(SucceedsThenFailsProvider {
        calls: Mutex::new(0),
        // The program goes away, as it does when someone closes the editor.
        then: || {
            FetchError::LocalSourceUnavailable(
                "no Antigravity language server or agy CLI process running".to_string(),
            )
        },
    })]);

    tick(&registry).await;
    assert!(
        registry.get_usage(None).await[0].usage.is_some(),
        "fixture must start healthy"
    );

    force_due(&registry, "local-lane");
    tick(&registry).await;

    let after = registry.get_usage(None).await;
    assert_eq!(after.len(), 1);
    assert!(
        after[0].usage.is_some(),
        "the cached window must survive: {:?}",
        after[0].error
    );
    assert!(
        after[0].error.is_none(),
        "a transient failure with a prior healthy window serves it stale rather \
         than replacing it with a degraded entry"
    );
}

/// The same fixture with an absent CREDENTIAL degrades instead.
///
/// This is the behaviour the local-source class deliberately does not share,
/// and asserting it here is what stops the test above from passing merely
/// because this fixture never degrades at all.
#[tokio::test]
async fn a_provider_whose_credential_is_absent_loses_its_window() {
    let registry = Registry::new(vec![Box::new(SucceedsThenFailsProvider {
        calls: Mutex::new(0),
        then: || FetchError::NoSession("no credential".to_string()),
    })]);

    tick(&registry).await;
    force_due(&registry, "local-lane");
    tick(&registry).await;

    let degraded = registry.get_usage(None).await;
    assert_eq!(
        degraded[0].error_class.as_deref(),
        Some("credential_absent"),
        "{:?}",
        degraded[0].error
    );
    assert!(degraded[0].usage.is_none());
}

/// A degraded entry carries the machine-readable class beside its prose.
///
/// The message is prose with no stability promise, and consumers are told not
/// to branch on it. Without the class travelling on the entry, a consumer that
/// needs to separate "nobody configured this provider" from "it is configured
/// and broken" has only the text to match on -- and that coupling breaks
/// silently whenever the wording is improved, in a direction neither side can
/// see: the producer experiences it as writing a clearer error, the consumer as
/// a user being told the wrong remedy.
///
/// Asserted at the wire, not on the slot. The class has been correct on the
/// slot for some time while never reaching a consumer, so "the value is
/// computed" and "the value is published" are separate claims and only the
/// second one matters here.
#[tokio::test]
async fn a_degraded_entry_publishes_its_error_class() {
    // Fails with NoSession, the class a consumer most needs to tell apart: it
    // means nobody logged in, not that anything broke.
    let registry = registry(&[("unconfigured", false, false)]);
    tick(&registry).await;

    let entries = registry.get_usage(None).await;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].error.is_some(), "the fixture must be degraded");
    assert_eq!(
        entries[0].error_class.as_deref(),
        Some("credential_absent"),
        "a degraded entry must name its class, not only its prose"
    );

    // Also checks the wire spelling, which is camelCase and differs from the
    // Rust field name. Serialization skips the field when it is None, so a
    // class that failed to be set produces a response missing the key rather
    // than one carrying null -- and a consumer reading an absent key cannot
    // tell that from a producer too old to send it.
    let json = serde_json::to_value(&entries[0]).unwrap();
    assert_eq!(
        json.get("errorClass").and_then(|v| v.as_str()),
        Some("credential_absent"),
        "published JSON: {json}"
    );
}

/// A healthy entry publishes no class at all.
///
/// The field answers why an entry is degraded, so a healthy entry has nothing
/// to say. Emitting an "ok" sentinel would make every consumer branch on a
/// value that means "ignore me", and absence is already the honest statement.
#[tokio::test]
async fn a_healthy_entry_publishes_no_error_class() {
    let registry = registry(&[("working", false, true)]);
    tick(&registry).await;

    let entries = registry.get_usage(None).await;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].error.is_none(), "the fixture must be healthy");
    assert_eq!(entries[0].error_class, None);

    let json = serde_json::to_value(&entries[0]).unwrap();
    assert!(
        json.get("errorClass").is_none(),
        "a healthy entry must omit the field entirely: {json}"
    );
}

#[tokio::test]
async fn entries_for_a_never_successful_slot_have_no_fetched_at() {
    // Only covers a slot that has NEVER succeeded. A degraded entry whose slot
    // succeeded earlier does carry a timestamp — see the test below, which is
    // the case a credential-poor host cannot produce.
    let registry = registry(&[("pending-or-degraded", false, false)]);
    assert!(registry.get_usage(None).await.is_empty());
    tick(&registry).await;
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].fetched_at.is_none());
}

/// A provider whose first fetch succeeds and whose later fetches fail
/// non-transiently, producing a degraded entry on a slot that has a recorded
/// success.
struct SucceedsThenAuthFailsProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for SucceedsThenAuthFailsProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::Unauthorized("401".into()))
        }
    }
}

#[tokio::test]
async fn a_degraded_entry_keeps_the_timestamp_of_its_last_success() {
    // The timestamp survives the failure that degrades the entry, so it reports
    // when the provider last SUCCEEDED — not when the error was observed, and not
    // when anything now in the entry was true. Consumers are told this, so it is
    // pinned here: presence of a timestamp says nothing about whether an entry is
    // usable, and the value must not be read as a failure time.
    //
    // This shape cannot be captured from a host lacking the credential, because
    // there every degraded provider has never succeeded and so carries no
    // timestamp at all — which is consistent with the opposite claim and is not
    // evidence for it. It has to be constructed.
    let registry = Registry::new(vec![Box::new(SucceedsThenAuthFailsProvider {
        calls: Mutex::new(0),
    })]);

    tick(&registry).await;
    let healthy = registry.get_usage(Some("codex")).await;
    let first_seen = healthy[0]
        .fetched_at
        .clone()
        .expect("a successful fetch stamps its entry");
    assert!(healthy[0].usage.is_some());

    force_due(&registry, "codex");
    tick(&registry).await;

    let degraded = registry.get_usage(Some("codex")).await;
    assert_eq!(degraded.len(), 1);
    assert!(
        degraded[0].error.is_some() && degraded[0].usage.is_none(),
        "the auth failure must degrade the entry"
    );
    assert_eq!(
        degraded[0].fetched_at.as_deref(),
        Some(first_seen.as_str()),
        "a degraded entry keeps its last success time, so the timestamp dates \
         content that is no longer present and must not be read as usability \
         or as when the failure happened"
    );
}

#[tokio::test]
async fn f8_stale_transient_success_serves_raw_usage_after_failure() {
    let registry = Registry::new(vec![Box::new(TransientlyFailingRelaxProvider {
        calls: AtomicUsize::new(0),
    })]);
    tick(&registry).await;
    assert_eq!(primary_percent(&registry.get_usage(None).await[0]), 0.0);

    force_due(&registry, "codex-stale-transient");
    tick(&registry).await;
    let served = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(primary_percent(&served[0]), 62.0);
    let stale = slot(&registry, "codex-stale-transient", "implicit-local");
    assert_eq!(stale.status, SlotStatus::StaleTransient);
    assert!(!stale.relax_eligible);
}

#[tokio::test]
async fn degraded_slots_and_other_providers_never_relax() {
    let registry = Registry::new(vec![
        Box::new(DegradingRelaxProvider {
            calls: AtomicUsize::new(0),
        }),
        Box::new(RelaxingProvider {
            name: "other-provider",
            eligible: false,
            usage: full_usage(31.0),
        }),
    ]);
    tick(&registry).await;
    let initial = registry.get_usage(None).await;
    assert_eq!(
        initial[1]
            .usage
            .as_ref()
            .unwrap()
            .primary
            .as_ref()
            .unwrap()
            .used_percent,
        31.0,
        "a provider that never opted in was transformed"
    );

    force_due(&registry, "codex-degraded");
    tick(&registry).await;
    let degraded = registry.get_usage(Some("codex-degraded")).await;
    assert_eq!(degraded.len(), 1);
    assert!(degraded[0].error.is_some());
    assert!(degraded[0].usage.is_none());
    assert!(!slot(&registry, "codex-degraded", "implicit-local").relax_eligible);
}

#[tokio::test]
async fn reporting_gate_keeps_wall_and_mutation_ticks_raw() {
    let temp = ResetTempDir::new("reporting-wall");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let now = reset_now();
    let facts = reset_facts(100.0, true);
    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, facts.clone()),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert!(result.consume_attempted);
    assert!(!result.relax_eligible);
    assert!(!reporting_eligible(true, &facts, true, false, true));
}

#[tokio::test]
async fn below_wall_expiry_mutation_tick_is_raw() {
    let temp = ResetTempDir::new("reporting-mutation");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(20.0, false)),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert!(result.trigger.expiry_trigger);
    assert!(result.consume_attempted);
    assert!(
        !result.relax_eligible,
        "mutation tick reported relaxed data"
    );
}

#[tokio::test]
async fn nothing_to_reset_storm_stays_raw_and_respects_spend_bound() {
    let temp = ResetTempDir::new("nothing-storm");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport =
        MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::NothingToReset));
    let now = reset_now();
    let first = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;
    let second = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert_eq!(first.outcome, Some(ConsumeOutcome::NothingToReset));
    assert!(!first.relax_eligible);
    assert!(!second.consume_attempted);
    assert!(!second.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spend_bound_delays_mutation_but_not_below_wall_truth_relaxation() {
    let temp = ResetTempDir::new("bound-not-reporting");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let now = reset_now();
    coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;

    let mut below_wall =
        reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(20.0, false));
    below_wall.earliest_expiry = Some(now + chrono::Duration::days(1));
    let result = coordinator
        .process_tick(
            "acct-reset",
            below_wall,
            &transport,
            &reset_request("token"),
        )
        .await;
    assert!(!result.consume_attempted);
    assert!(result.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[test]
fn corrupt_journal_prevents_reset_coordinator_construction() {
    let temp = ResetTempDir::new("corrupt-process");
    let journal = temp.journal();
    std::fs::write(journal.path(), b"{").unwrap();

    assert!(ResetCoordinator::new(journal).is_err());
}

#[cfg(unix)]
#[test]
fn f5_read_only_journal_parent_disarms_before_reporting_can_relax() {
    use std::os::unix::fs::PermissionsExt;

    let temp = ResetTempDir::new("read-only-journal");
    let parent = temp.journal().path().parent().unwrap().to_path_buf();
    let original = std::fs::metadata(&parent).unwrap().permissions();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = ResetCoordinator::new(temp.journal());
    std::fs::set_permissions(&parent, original).unwrap();

    assert!(result.is_err());
}

struct CreditsFailurePathProvider {
    transport: MockResetTransport,
}

#[async_trait]
impl UsageProvider for CreditsFailurePathProvider {
    fn name(&self) -> &str {
        "codex-credits-failure"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let usage_snapshot = crate::codex::normalize_usage_snapshot(
            br#"{"rate_limit":{"primary_window":{"used_percent":73.0},"limit_reached":false}}"#,
        )
        .unwrap();
        let credits = self.transport.fetch_credits(&reset_request("token")).await;
        match crate::codex::normalize_credits_tick(credits, Utc::now()) {
            Ok(_) => panic!("the mock credits request unexpectedly armed the provider"),
            Err(_) => crate::codex::unarmed_usage_attempt(
                observed(Some("acct-reset")),
                "oauth",
                usage_snapshot,
            ),
        }
    }
}

#[tokio::test]
async fn f8_credits_get_failure_follows_provider_path_and_keeps_raw_percentages() {
    let registry = Registry::new(vec![Box::new(CreditsFailurePathProvider {
        transport: MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset))
            .credits_failure(),
    })]);
    tick(&registry).await;
    let served = registry.get_usage(None).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("acct-reset"));
    assert_eq!(primary_percent(&served[0]), 73.0);
    assert!(!slot(&registry, "codex-credits-failure", "implicit-local").relax_eligible);
}

#[tokio::test]
async fn f6_missing_limit_reached_never_relaxes_even_at_low_usage() {
    let snapshot = crate::codex::normalize_usage_snapshot(
        br#"{"rate_limit":{"primary_window":{"used_percent":10.0,"reset_at":1900000000,"limit_window_seconds":3600}}}"#,
    )
    .unwrap();
    assert_eq!(snapshot.limit_reached, None);
    let facts = UsageFacts::from_usage(&snapshot.usage, snapshot.limit_reached);
    assert!(!facts.at_wall);
    assert!(!facts.below_wall());

    let temp = ResetTempDir::new("missing-wall-evidence");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let mut input = reset_tick_input(reset_now(), facts);
    input.earliest_expiry = Some(input.now + chrono::Duration::days(20));
    let result = coordinator
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;
    assert!(!result.relax_eligible);
    assert!(!result.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}

#[test]
fn f3_trigger_uses_earliest_credit_outside_the_safety_margin() {
    let now = reset_now();
    let credit = |id: &str, expiry: chrono::DateTime<Utc>| {
        format!(
            r#"{{"id":"{id}","reset_type":"codex_rate_limits","status":"available","expires_at":"{}"}}"#,
            expiry.to_rfc3339()
        )
    };
    let mixed = normalize_credits(
        format!(
            r#"{{"credits":[{},{}],"available_count":2}}"#,
            credit("inside-margin", now + chrono::Duration::seconds(30)),
            credit("healthy", now + chrono::Duration::days(20))
        )
        .as_bytes(),
    )
    .unwrap();
    let mixed_expiry = crate::codex::reset_trigger_expiry(&mixed, now);
    let mixed_trigger = evaluate_trigger(&TriggerInput {
        armed: mixed_expiry.is_some(),
        now,
        earliest_expiry: mixed_expiry,
        auto_use_resets_secs: 24 * 60 * 60,
        any_used_floor: true,
        at_wall: false,
        pending: false,
        spend_bound_allows: true,
        before_post_cutoff: true,
    });
    assert!(!mixed_trigger.expiry_trigger);
    assert!(!mixed_trigger.fire);

    let near = normalize_credits(
        format!(
            r#"{{"credits":[{}],"available_count":1}}"#,
            credit("healthy-near", now + chrono::Duration::hours(12))
        )
        .as_bytes(),
    )
    .unwrap();
    let near_expiry = crate::codex::reset_trigger_expiry(&near, now);
    assert!(
        evaluate_trigger(&TriggerInput {
            armed: near_expiry.is_some(),
            now,
            earliest_expiry: near_expiry,
            auto_use_resets_secs: 24 * 60 * 60,
            any_used_floor: true,
            at_wall: false,
            pending: false,
            spend_bound_allows: true,
            before_post_cutoff: true,
        })
        .fire
    );
}

#[test]
fn zero_or_expiring_now_credits_cannot_arm() {
    let now = reset_now();
    let zero = normalize_credits(br#"{"credits":[],"available_count":0}"#).unwrap();
    assert_eq!(zero.earliest_usable_expiry(now), None);

    let expiring = normalize_credits(
        br#"{
            "credits": [{
                "id": "credit-soon",
                "status": "available",
                "expires_at": "2026-07-14T12:00:30Z"
            }],
            "available_count": 1
        }"#,
    )
    .unwrap();
    assert_eq!(expiring.earliest_usable_expiry(now), None);
    assert!(!reporting_eligible(
        false,
        &reset_facts(20.0, false),
        false,
        false,
        true
    ));
}

#[tokio::test]
async fn consume_response_codes_http_error_and_timeout_are_fail_closed() {
    for outcome in [
        ConsumeOutcome::Reset,
        ConsumeOutcome::NothingToReset,
        ConsumeOutcome::NoCredit,
        ConsumeOutcome::AlreadyRedeemed,
    ] {
        let temp = ResetTempDir::new(outcome.as_code());
        let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
        let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(outcome));
        let result = coordinator
            .process_tick(
                "acct-reset",
                reset_tick_input(reset_now(), reset_facts(100.0, true)),
                &transport,
                &reset_request("token"),
            )
            .await;
        assert_eq!(result.outcome, Some(outcome));
        assert!(!result.pending);
        assert!(!result.relax_eligible);
    }

    let error_temp = ResetTempDir::new("http-error");
    let error_coordinator = ResetCoordinator::new(error_temp.journal()).unwrap();
    let error_transport = MockResetTransport::new(MockConsumeBehavior::Error);
    let error_result = error_coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &error_transport,
            &reset_request("token"),
        )
        .await;
    assert!(error_result.consume_attempted);
    assert!(error_result.pending);
    assert_eq!(error_result.outcome, None);
    assert!(!error_result.relax_eligible);

    let timeout_temp = ResetTempDir::new("timeout");
    let timeout_coordinator = ResetCoordinator::new(timeout_temp.journal()).unwrap();
    let timeout_transport = MockResetTransport::new(MockConsumeBehavior::Hang);
    let timeout_result = timeout_coordinator
        .process_tick_with_timeout(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &timeout_transport,
            &reset_request("token"),
            Duration::from_millis(10),
        )
        .await;
    assert!(timeout_result.consume_attempted);
    assert!(timeout_result.pending);
    assert_eq!(timeout_result.outcome, None);
    assert!(!timeout_result.relax_eligible);
}

#[test]
fn credits_date_header_is_the_arming_clock_when_valid() {
    let local = Utc.with_ymd_and_hms(2026, 7, 10, 1, 2, 3).single().unwrap();
    let server = response_now(Some("Tue, 14 Jul 2026 12:00:00 GMT"), local);
    assert_eq!(server, reset_now());
    assert_eq!(response_now(Some("not-a-date"), local), local);
}

#[test]
fn credits_normalizer_matches_live_captured_contract() {
    let credits = normalize_credits(
        br#"{
            "credits": [
                {
                    "id": "rc_01",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2026-06-14T12:00:00Z",
                    "expires_at": "2026-07-14T13:00:01Z",
                    "redeem_started_at": null,
                    "redeemed_at": null,
                    "title": "Full reset",
                    "description": "Reset Codex rate limits"
                },
                {
                    "id": "rc_02",
                    "reset_type": "codex_rate_limits",
                    "status": "redeemed",
                    "granted_at": "2026-06-01T12:00:00Z",
                    "expires_at": "2026-07-01T12:00:00Z",
                    "redeem_started_at": "2026-06-20T12:00:00Z",
                    "redeemed_at": "2026-06-20T12:00:01Z",
                    "title": "Full reset",
                    "description": "Reset Codex rate limits"
                }
            ],
            "available_count": 1
        }"#,
    )
    .unwrap();

    assert_eq!(credits.reported_available_count, 1);
    assert_eq!(credits.available.len(), 1);
    assert_eq!(credits.available[0].id, "rc_01");
    assert_eq!(
        credits.available[0].expires_at,
        Utc.with_ymd_and_hms(2026, 7, 14, 13, 0, 1)
            .single()
            .unwrap()
    );
    assert_eq!(credits.saved_resets().available_count, 1);
    // Nine fractional digits even though this expiry is a whole second. The
    // captured payload is the evidence that the truncating default reaches real
    // data: upstream credit expiries land on whole seconds routinely, so under
    // `to_rfc3339` this string had no fractional part while a sibling with
    // non-zero nanoseconds carried nine digits, and one instant had two
    // spellings on the same response.
    assert_eq!(
        credits.saved_resets().soonest_expires_at.as_deref(),
        Some("2026-07-14T13:00:01.000000000+00:00")
    );
    assert_eq!(credits.saved_resets().credits.len(), 1);
}

#[tokio::test]
async fn a_saturated_provider_still_advances_its_later_handles() {
    // Enough due providers to fill the concurrency cap on the first pass, so each
    // provider gets roughly one admission per turn while holding two due handles.
    //
    // This is the pressure case: if admission order were fixed, the same handle
    // would be taken every turn and the second would never be reached, leaving it
    // permanently unattempted. An unattempted handle has no resolved account, and
    // the read path collapses a provider to a single unlabeled entry unless every
    // handle resolves one — so the visible cost is that the provider's accounts
    // stop being reported separately.
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: 2,
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    // Sustained pressure: a handle that was just refreshed falls due again before
    // the next turn, which is what a refresh interval shorter than a full turn
    // looks like. Slots that have NOT been attempted keep their original due time,
    // because nothing has refreshed them — overwriting it would erase how long
    // they have been waiting, which is the signal fair admission depends on.
    for _ in 0..6 {
        tick(&registry).await;
        let mut store = registry.store.lock().unwrap();
        for (key, mut slot) in store.snapshot() {
            if slot.last_success_at.is_none() {
                continue;
            }
            slot.next_due_at = Instant::now();
            let incarnation = slot.incarnation;
            let attempt_sequence = slot.attempt_sequence;
            assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
        }
    }

    let store = registry.store.lock().unwrap();
    let never_attempted: Vec<_> = store
        .snapshot()
        .into_iter()
        .filter(|(_, slot)| slot.status == SlotStatus::Pending)
        .map(|(key, _)| format!("{}/{}", key.provider, key.handle.stable_id()))
        .collect();

    assert!(
        never_attempted.is_empty(),
        "every handle must eventually be admitted under sustained pressure; \
         these were never attempted: {never_attempted:?}"
    );
}

/// Two handles resolving the same account, both serving stale data after their
/// own transient failures, with different observation ages.
struct StaleDuplicateProvider;

#[async_trait]
impl UsageProvider for StaleDuplicateProvider {
    fn name(&self) -> &str {
        "stale-dup"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        // Deliberately ordered so the handle carrying the OLDER snapshot sorts
        // first: if selection fell back to handle order, it would win.
        Ok(vec![handle("A-early"), handle("B-late")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let used_percent = if handle.stable_id() == "A-early" {
            10.0
        } else {
            90.0
        };
        let usage = Usage {
            primary: Some(RateWindow {
                used_percent,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
            }),
            ..Usage::default()
        };
        FetchAttempt::success(observed(Some("shared")), "test", usage)
    }
}

/// The same two handles, but the newer snapshot reports LESS usage.
///
/// Reachable whenever a window resets between two fetches, which is the routine
/// case for a five-hour window on a duplicated account.
struct ResetDuplicateProvider;

#[async_trait]
impl UsageProvider for ResetDuplicateProvider {
    fn name(&self) -> &str {
        "reset-dup"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("A-early"), handle("B-late")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        // The older handle saw the window nearly full; the newer one saw it
        // after the reset.
        let used_percent = if handle.stable_id() == "A-early" {
            95.0
        } else {
            4.0
        };
        let usage = Usage {
            primary: Some(RateWindow {
                used_percent,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
            }),
            ..Usage::default()
        };
        FetchAttempt::success(observed(Some("shared")), "test", usage)
    }
}

/// Recency decides the duplicate, even when the newer snapshot reports less.
///
/// Two handles can resolve the same account, and when both are stale-serving
/// the tiebreak is which observation is more recent. It is tempting to justify
/// that by saying usage only grows within a window, so the older snapshot
/// understates pressure — but a window that resets between the two fetches
/// makes the newer snapshot lower, and it is still the one to serve, because
/// the older one describes a state the account has left.
///
/// Without this case a rule of "prefer the higher percent" passes every other
/// test, and it would pin an account at its pre-reset figure until the older
/// handle succeeded again.
#[tokio::test]
async fn duplicate_account_serves_the_newer_observation_after_a_reset() {
    let registry = Registry::new(vec![Box::new(ResetDuplicateProvider)]);
    tick(&registry).await;

    {
        let mut store = registry.store.lock().unwrap();
        for (key, mut slot) in store.snapshot() {
            slot.status = SlotStatus::StaleTransient;
            let age_secs = if key.handle.stable_id() == "A-early" {
                600
            } else {
                5
            };
            slot.last_success_at = Some(Instant::now() - Duration::from_secs(age_secs));
            let incarnation = slot.incarnation;
            let attempt_sequence = slot.attempt_sequence;
            assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
        }
    }

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1, "one entry per account");
    assert_eq!(
        usage[0]
            .usage
            .as_ref()
            .and_then(|usage| usage.primary.as_ref())
            .map(|window| window.used_percent),
        Some(4.0),
        "the newer observation must win even when it reports less usage"
    );
}

#[tokio::test]
async fn duplicate_account_with_equal_status_serves_the_newer_observation() {
    let registry = Registry::new(vec![Box::new(StaleDuplicateProvider)]);
    tick(&registry).await;

    // Both slots end up stale-serving, but one observation is ten minutes older
    // than the other. The newer one wins because it is newer, not because of
    // what it reports: the older snapshot describes a state the account has
    // already left, and only the newer one can reflect a reset.
    {
        let mut store = registry.store.lock().unwrap();
        for (key, mut slot) in store.snapshot() {
            slot.status = SlotStatus::StaleTransient;
            let age_secs = if key.handle.stable_id() == "A-early" {
                600
            } else {
                5
            };
            slot.last_success_at = Some(Instant::now() - Duration::from_secs(age_secs));
            let incarnation = slot.incarnation;
            let attempt_sequence = slot.attempt_sequence;
            assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
        }
    }

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1, "one entry per account");
    assert_eq!(
        usage[0]
            .usage
            .as_ref()
            .and_then(|usage| usage.primary.as_ref())
            .map(|window| window.used_percent),
        Some(90.0),
        "the newer observation must win; serving the older one understates usage"
    );
}

#[tokio::test]
async fn a_provider_that_resolves_no_handle_is_named_rather_than_skipped() {
    // A provider whose credential enumeration keeps failing holds no slots, so it
    // lands in none of the fresh/stale/degraded buckets while still counting
    // toward providers_total. Skipping it silently would let the buckets
    // under-sum, and "configured but never came up" would read as an absence
    // rather than as a problem worth acting on.
    let provider = EnumerationProvider {
        handles: Arc::new(Mutex::new(vec![handle("H1")])),
        fail_enumeration: Arc::new(Mutex::new(true)),
        calls: Arc::new(Mutex::new(HashMap::new())),
    };
    let registry = Registry::new(vec![Box::new(provider)]);

    // Before the first tick every provider legitimately has no slots yet, so
    // reporting then would name all of them for the first seconds after start.
    assert!(
        registry.health().without_handles.is_empty(),
        "a provider must not be reported as handle-less before the refresher runs"
    );

    tick(&registry).await;

    let health = registry.health();
    assert_eq!(
        health.without_handles,
        vec!["enumerated"],
        "a provider that resolved no handle must be visible by name"
    );
    assert_eq!(health.fresh + health.stale + health.degraded.len(), 0);
    assert_eq!(
        health.providers_total, 1,
        "it still counts in the total, which is why its absence must be reported"
    );
}

#[tokio::test]
async fn an_ineligible_slot_is_served_raw_even_when_fresh() {
    // The relaxation transform GRANTS a capacity claim: it reports 0% used while
    // the provider actually reported far more, so that a router treats the account
    // as having room. Two conditions must BOTH hold before that claim is made —
    // the slot opted in, and it is fresh.
    //
    // This pins the opt-in half directly. The freshness half has its own test, but
    // dropping the eligibility check reddened only unrelated happy-path tests,
    // which means the condition was defended by accident rather than on purpose.
    // A grant that is only incidentally guarded is one refactor away from being
    // handed out unearned, and the failure is silent: the wire would assert spare
    // capacity that does not exist.
    let registry = Registry::new(vec![Box::new(RelaxingProvider {
        name: "never-opted-in",
        eligible: false,
        usage: full_usage(88.0),
    })]);
    tick(&registry).await;

    let entries = registry.get_usage(Some("never-opted-in")).await;
    let primary = entries[0]
        .usage
        .as_ref()
        .expect("a healthy window")
        .primary
        .as_ref()
        .expect("primary window");

    assert_eq!(
        primary.used_percent, 88.0,
        "a slot that never opted in must be served exactly as the provider reported"
    );
    assert_eq!(
        primary.raw_used_percent, None,
        "the raw annotation marks a relaxed window, so an untransformed one must \
         not carry it"
    );
}

struct LabeledIneligibleProvider;

#[async_trait]
impl UsageProvider for LabeledIneligibleProvider {
    fn name(&self) -> &str {
        "codex-labeled-ineligible"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(observed(Some("codex-account")), "test", full_usage(73.0))
    }
}

#[tokio::test]
async fn an_ineligible_labeled_slot_is_served_raw() {
    // The labeled path is where banked resets actually operate, since arming
    // requires a resolved account. It has a test proving it DOES relax when the
    // slot opted in; this pins the other side, that it does not otherwise.
    //
    // Both emission paths grant the same claim and each needs its own guard: a
    // test on one says nothing about the other, and the labeled branch is the one
    // that matters most, because a router reads a labeled entry as a specific
    // account's spare capacity.
    let registry = Registry::new(vec![Box::new(LabeledIneligibleProvider)]);
    tick(&registry).await;

    let served = registry.get_usage(Some("codex-labeled-ineligible")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("codex-account"));
    assert_eq!(
        primary_percent(&served[0]),
        73.0,
        "a labeled slot that never opted in must report the provider's own number"
    );
}

#[tokio::test]
async fn a_labeled_entry_is_withheld_while_its_identity_is_in_flux() {
    // Attaching an account to an entry GRANTS a claim: it says this usage
    // belongs to that account, and a consumer routes and meters on it. The claim
    // must be withheld while identity is unconfirmed, because the alternative is
    // attributing one account's usage to another — the most expensive thing this
    // module can get wrong, and undetectable downstream.
    //
    // The unlabeled path has its own test for this. The labeled path is the one
    // that carries an account on the wire, so it needs its own: the guards are
    // separate expressions and a test on one proves nothing about the other.
    let registry = Registry::new(vec![Box::new(LabeledRelaxingProvider)]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(Some("codex-labeled")).await.len(),
        1,
        "precondition: the account resolved and is served"
    );

    let key = SlotKey::new("codex-labeled", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        assert!(
            slot.entry.is_some(),
            "a cached entry is what makes this a risk"
        );
        assert!(
            slot.account_id().is_some(),
            "precondition: an account label exists to be withheld"
        );
        slot.label_in_flux = true;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    assert!(
        registry.get_usage(Some("codex-labeled")).await.is_empty(),
        "a cached entry whose identity is unconfirmed must not be served under \
         its previous account label"
    );
}

/// Two lanes of one provider, both healthy, publishing different window sets.
///
/// The windows are named so the selected lane is identifiable. Two lanes can
/// describe the same account at different granularities: `antigravity` reads
/// per-pool detail from a local process and pooled summaries from its cloud API,
/// folding a flat per-model response back into pools, so the two do not publish
/// an identical set of windows.
struct TwoLaneProvider;

fn named_window(id: &str) -> Usage {
    Usage {
        extra_rate_windows: Some(vec![crate::model::ExtraWindow {
            id: Some(id.to_string()),
            title: Some(id.to_string()),
            window: Some(crate::model::RateWindow {
                used_percent: 10.0,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: None,
                used_count: None,
                total_count: None,
            }),
        }]),
        ..Usage::default()
    }
}

#[async_trait]
impl UsageProvider for TwoLaneProvider {
    fn name(&self) -> &str {
        "two-lane"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![
            CredentialHandle::implicit(),
            CredentialHandle::vault(
                "remote:lane",
                crate::credential_source::VaultCapability::new("ckh_two_lane"),
            ),
        ])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        // Neither lane resolves an account id, which is the case for a provider
        // whose upstream does not identify the account -- so both entries are
        // unlabeled and the read path must pick exactly one.
        let lane = if handle.is_local() { "local" } else { "remote" };
        FetchAttempt::success(observed(None), "test", named_window(lane))
    }
}

/// When both lanes of one provider are healthy, the local lane is served.
///
/// Two handles that resolve no account are indistinguishable to a consumer, so
/// exactly one entry is published and something has to choose it. Freshness and
/// health decide first; when those tie -- which is the ordinary case once both
/// lanes are working -- the tie is broken toward the local one.
///
/// This became observable only once a provider gained two lanes that report
/// DIFFERENT window sets. While every provider's lanes published the same shape,
/// the choice could not be seen from the wire and any order looked correct. Now
/// the winner decides which windows a consumer receives, so leaving it to handle
/// ordering would make the published shape depend on an incidental detail.
///
/// Local is preferred because it does not spend a credential: it reads a process
/// or file on this machine, where the other lane costs a vault lookup and a
/// network round trip to learn the same thing.
#[tokio::test]
async fn the_local_lane_wins_when_both_lanes_of_a_provider_are_healthy() {
    let registry = Registry::new(vec![Box::new(TwoLaneProvider)]);
    tick(&registry).await;

    let entries = registry.get_usage(Some("two-lane")).await;

    // Exactly one entry: two unlabeled entries for one provider are
    // indistinguishable to anything keying on (provider, account).
    assert_eq!(entries.len(), 1, "{entries:#?}");

    let ids: Vec<&str> = entries[0]
        .usage
        .as_ref()
        .and_then(|usage| usage.extra_rate_windows.as_ref())
        .map(|extras| extras.iter().filter_map(|x| x.id.as_deref()).collect())
        .unwrap_or_default();
    assert_eq!(
        ids,
        vec!["local"],
        "the remote lane won a tie against local"
    );
}

/// The provider count stated in the matrix is the count the registry builds.
///
/// A number restated in prose drifts silently: the registry gains a provider,
/// the sentence does not, and nothing disagrees. This one was already wrong by
/// one -- written by counting `Box::new` lines in the registry source, which
/// also matches a `Box::new` that wraps a fetch outcome and registers nothing.
///
/// Checked here rather than noted, because a claim about another file is a
/// comment pretending to be a guarantee until something compares the two. The
/// assertion names both sides so a failure says which to change: adding a
/// provider should update the sentence, not this test.
#[test]
fn the_documented_provider_count_matches_the_registry() {
    let registered = Registry::with_defaults(crate::config::QuotaConfig::default(), None)
        .provider_names()
        .len();

    let matrix = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/provider-matrix.md"),
    )
    .expect("provider-matrix.md must be readable from the crate directory");

    let stated: usize = matrix
        .split_once(" providers registered")
        .and_then(|(before, _)| {
            before
                .rsplit(|c: char| !c.is_ascii_digit())
                .find(|token| !token.is_empty())
                .and_then(|token| token.parse().ok())
        })
        .expect("the matrix must state a provider count as `<N> providers registered`");

    assert_eq!(
        stated, registered,
        "docs/provider-matrix.md states {stated} providers and the registry builds \
         {registered}; update the matrix sentence when adding or removing a provider"
    );
}

/// A credential reaching no account is named in health, not just in the array.
///
/// The provider serves normally, so it lands in `fresh` and every count on the
/// snapshot reads healthy. The only other evidence is its absence from
/// `completeProviders` on `usage.get` — a signal a reader has to know to look
/// for, on a different call. This is the surface an operator sees without
/// asking for anything.
///
/// The state is the one a handle enters when its credential is deleted while
/// the handle stays configured: it can never resolve again and nothing about it
/// changes on its own.
#[tokio::test]
async fn a_credential_reaching_no_account_is_named_in_health() {
    // Vault handles: the metric is about a credential somebody minted, so a
    // fixture built from implicit local lanes would exercise the wrong shape and
    // pass for the wrong reason.
    let provider =
        CompletenessProvider::new(&["H1", "H2"], &[("H1", Some("A")), ("H2", Some("B"))]);
    *provider.handles.lock().unwrap() = Ok(vec![
        CredentialHandle::vault(
            "H1",
            crate::credential_source::VaultCapability::new("ckh_h1"),
        ),
        CredentialHandle::vault(
            "H2",
            crate::credential_source::VaultCapability::new("ckh_h2"),
        ),
    ]);
    let fail = Arc::clone(&provider.fail);
    let labels = Arc::clone(&provider.labels);
    let registry = Registry::new(vec![Box::new(provider)]);

    fail.lock().unwrap().insert("H2".to_string());
    labels.lock().unwrap().insert("H2".into(), None);
    tick(&registry).await;

    let health = registry.health();
    assert_eq!(
        health.handles_without_account,
        vec!["complete".to_string()],
        "a credential reaching no account must be named"
    );
    // Still counted exactly once in the buckets: this line sits beside the
    // conservation identity rather than inside it.
    assert_eq!(
        health.fresh, 1,
        "the provider is serving and counts as fresh"
    );
    assert!(health.degraded.is_empty(), "and is not a fault");

    // Silent when every handle resolves, or the line would name every
    // multi-account provider and stop being a signal.
    //
    // A second registry rather than repairing this one: a non-transient failure
    // puts the slot on a fixed multi-minute backoff, so another tick here would
    // not re-fetch it and the control would be testing the backoff instead of
    // the metric.
    let healthy = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1", "H2"],
        &[("H1", Some("A")), ("H2", Some("B"))],
    ))]);
    tick(&healthy).await;
    assert!(
        healthy.health().handles_without_account.is_empty(),
        "a provider whose credentials all resolve must not be named"
    );

    // Silent for an implicit local lane that resolves nothing. Most providers
    // ship one -- an environment variable or a file path -- and it exists
    // whether or not anyone uses it, so on a host that does not, it fails with
    // an absent credential and no identity while a minted handle beside it
    // serves. Naming that reports a provider whose only fault is offering a lane
    // nobody configured.
    let mixed = CompletenessProvider::new(&["V1", "L1"], &[("V1", Some("A")), ("L1", None)]);
    *mixed.handles.lock().unwrap() = Ok(vec![
        CredentialHandle::vault(
            "V1",
            crate::credential_source::VaultCapability::new("ckh_v1"),
        ),
        handle("L1"),
    ]);
    mixed.fail.lock().unwrap().insert("L1".to_string());
    let with_local = Registry::new(vec![Box::new(mixed)]);
    tick(&with_local).await;
    let local_health = with_local.health();
    assert!(
        local_health.handles_without_account.is_empty(),
        "an unconfigured local lane beside a serving vault handle must not be named"
    );
    assert_eq!(
        local_health.fresh, 1,
        "and the provider is serving, so the control is not vacuous"
    );

    // Silent for a provider that serves usage while resolving no identity. Many
    // upstreams return no account id at all, so their entries are healthy and
    // permanently unlabeled — naming those would put most of the registry on
    // this line on every host and make it noise. The line is about a credential
    // that FAILED to reach an account, not one whose upstream has no account to
    // name.
    let anonymous = Registry::new(vec![Box::new(CompletenessProvider::new(
        &["H1"],
        &[("H1", None)],
    ))]);
    tick(&anonymous).await;
    let anonymous_health = anonymous.health();
    assert!(
        anonymous_health.handles_without_account.is_empty(),
        "a healthy provider that reports no account id must not be named"
    );
    assert_eq!(
        anonymous_health.fresh, 1,
        "and it is serving, so the control is not vacuous"
    );

    // Silent when NO account resolves, which is a different fault with its own
    // line. Without this the metric would repeat every provider already named
    // by `unconfigured` or `degraded` — on a host where most adapters have no
    // credential that is most of the registry, and the case nothing else
    // reports would be buried in a list nobody reads.
    let all_failing = CompletenessProvider::new(&["H1", "H2"], &[("H1", None), ("H2", None)]);
    all_failing.fail.lock().unwrap().insert("H1".to_string());
    all_failing.fail.lock().unwrap().insert("H2".to_string());
    let dark = Registry::new(vec![Box::new(all_failing)]);
    tick(&dark).await;
    let dark_health = dark.health();
    assert!(
        dark_health.handles_without_account.is_empty(),
        "a provider with no serving account is reported elsewhere, not here"
    );
    assert!(
        !dark_health.degraded.is_empty() || !dark_health.unconfigured.is_empty(),
        "and it must be reported by one of those lines, or it vanishes entirely"
    );
}

/// A preserved reading says it is preserved, and says since when.
///
/// Without this an entry served through an ongoing failure is byte-identical to
/// a fresh one apart from `fetchedAt`, so a consumer cannot separate "old
/// because the provider is unreachable" from "old because nothing polled". The
/// two have opposite remedies, and a consumer with only a timestamp has to guess
/// with a wall-clock threshold that denies fresh-enough data to catch stale
/// data.
#[tokio::test]
async fn a_reading_served_through_a_failure_says_so() {
    struct FlakyProvider {
        healthy: Arc<AtomicBool>,
    }

    #[async_trait]
    impl UsageProvider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
            if self.healthy.load(Ordering::SeqCst) {
                FetchAttempt::success(None, "api", Usage::default())
            } else {
                // Transient, so the prior window is preserved rather than
                // replaced by a degraded entry -- the state under test.
                FetchAttempt::failure(None, None, FetchError::Upstream("HTTP 503".into()))
            }
        }
    }

    let healthy = Arc::new(AtomicBool::new(true));
    let registry = Registry::new(vec![Box::new(FlakyProvider {
        healthy: Arc::clone(&healthy),
    })]);

    tick(&registry).await;
    let fresh = registry.get_usage(None).await;
    assert_eq!(fresh.len(), 1);
    assert!(
        fresh[0].stale.is_none(),
        "a successful read must not claim to be stale: {:?}",
        fresh[0].stale
    );

    // The upstream starts failing transiently, so the last good window keeps
    // being served rather than blanked.
    healthy.store(false, Ordering::SeqCst);
    force_due(&registry, "flaky");
    tick(&registry).await;

    let served = registry.get_usage(None).await;
    assert_eq!(served.len(), 1);
    assert!(
        served[0].usage.is_some(),
        "the preserved window is still served"
    );
    assert!(
        served[0].error.is_none(),
        "and it is not a degraded entry -- which is exactly why the disclosure is needed"
    );
    let stale = served[0]
        .stale
        .as_ref()
        .expect("a preserved reading must disclose that it is one");
    assert_eq!(
        stale.class.as_deref(),
        Some("upstream_failed"),
        "the failure class travels with the disclosure"
    );

    // `since` is the failure, `fetchedAt` is the reading. Conflating them is the
    // mistake the field exists to prevent, so they must not be equal.
    let fetched_at = served[0]
        .fetched_at
        .as_deref()
        .expect("a preserved reading carries the timestamp of the read it preserves");
    assert_ne!(
        stale.since, fetched_at,
        "the two timestamps answer different questions"
    );
    assert!(
        stale.since.as_str() > fetched_at,
        "the failure began after the reading was taken: since={} fetched_at={fetched_at}",
        stale.since
    );

    // A SECOND failure must not move `since`. This is the whole point of the
    // field: during an outage the last attempt is always seconds ago, so
    // reporting that would describe the entry as freshly checked at the moment
    // it is least trustworthy. One failure cannot show this, because the first
    // attempt and the run's start are the same instant.
    force_due(&registry, "flaky");
    tick(&registry).await;
    let still_failing = registry.get_usage(None).await;
    let later = still_failing[0]
        .stale
        .as_ref()
        .expect("still preserved after a second failure");
    assert_eq!(
        later.since, stale.since,
        "`since` marks the start of the failure run, not the latest attempt"
    );

    // Recovery clears it, or a provider that flapped once would look stale for
    // the rest of its life.
    healthy.store(true, Ordering::SeqCst);
    force_due(&registry, "flaky");
    tick(&registry).await;
    let recovered = registry.get_usage(None).await;
    assert!(
        recovered[0].stale.is_none(),
        "a recovered entry must stop claiming staleness: {:?}",
        recovered[0].stale
    );
}

/// One instant always renders as one string.
///
/// `to_rfc3339` picks precision from the value: a whole number of seconds
/// prints no fractional part, nanoseconds ending in three zeros print six
/// digits, everything else prints nine. All valid RFC 3339, all from the same
/// call, so the variation reads as a code path difference when it is
/// arithmetic.
///
/// Invisible while the timestamp is only a freshness hint, load-bearing the
/// moment a consumer keys on it: several spellings of one instant means string
/// equality is not instant equality, so a dedupe key on the raw string admits
/// duplicates a parsed comparison would catch. The values below are the two
/// that lose digits under the default.
#[test]
fn a_wire_timestamp_has_one_spelling_per_instant() {
    use chrono::TimeZone;

    let whole_second = chrono::Utc.timestamp_opt(1_760_000_000, 0).unwrap();
    let trailing_zeros = chrono::Utc
        .timestamp_opt(1_760_000_000, 98_805_000)
        .unwrap();
    let full_nanos = chrono::Utc
        .timestamp_opt(1_760_000_000, 98_805_042)
        .unwrap();

    assert_eq!(
        crate::rfc3339_canonical(whole_second),
        "2025-10-09T08:53:20.000000000+00:00",
        "a whole second must not drop the fractional part entirely"
    );
    assert_eq!(
        crate::rfc3339_canonical(trailing_zeros),
        "2025-10-09T08:53:20.098805000+00:00",
        "trailing zero nanoseconds must not shorten the string to microseconds"
    );
    assert_eq!(
        crate::rfc3339_canonical(full_nanos),
        "2025-10-09T08:53:20.098805042+00:00"
    );

    // Every spelling is the same length, which is the property a consumer keying
    // on the string depends on and the one the default formatter breaks.
    let lengths: std::collections::HashSet<usize> = [whole_second, trailing_zeros, full_nanos]
        .into_iter()
        .map(|timestamp| crate::rfc3339_canonical(timestamp).len())
        .collect();
    assert_eq!(
        lengths.len(),
        1,
        "one instant, one spelling: got {lengths:?} distinct lengths"
    );

    // And they still parse back to what they came from, so pinning the precision
    // did not trade a formatting bug for a correctness one.
    for timestamp in [whole_second, trailing_zeros, full_nanos] {
        let rendered = crate::rfc3339_canonical(timestamp);
        let parsed = chrono::DateTime::parse_from_rfc3339(&rendered)
            .expect("the canonical form must be valid RFC 3339");
        assert_eq!(parsed.with_timezone(&chrono::Utc), timestamp);
    }
}
