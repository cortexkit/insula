//! quota-core — provider usage fetchers + RateWindow normalization + TTL cache.
//!
//! Wire-agnostic: this crate knows nothing about subc. It exposes a [`Registry`]
//! that fetches per-provider usage (reusing each provider's own session) and
//! assembles the silent-degrade `/usage` array Alfonso consumes. The subc module
//! (`quota-module`) wraps this behind the `usage.get` route op.

pub mod alibaba;
pub mod amp;
pub mod anthropic;
pub mod antigravity;
pub mod browser_cookies;
pub mod cache;
pub mod codebuff;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod doubao;
pub mod elevenlabs;
pub mod env;
pub mod factory;
pub mod gemini;
pub mod grok;
pub mod health;
pub mod http;
pub mod jetbrains;
pub mod kilo;
pub mod kimi;
pub mod llmproxy;
pub mod manus;
pub mod mimo;
pub mod minimax;
pub mod model;
pub mod ollama;
pub mod opencode;
pub mod opencode_auth;
pub mod opencodego;
pub mod provider;
pub mod stepfun;
pub mod synthetic;
pub mod warp;
pub mod zai;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use cache::UsageCache;
use health::HealthSnapshot;
use model::ProviderUsage;
use provider::UsageProvider;

/// The provider registry: holds the fetchers and the response cache, and serves
/// the `usage.get` query.
pub struct Registry {
    providers: Vec<Box<dyn UsageProvider>>,
    cache: Mutex<UsageCache>,
}

impl Registry {
    /// Build a registry from a provider set with the given cache TTL.
    pub fn new(providers: Vec<Box<dyn UsageProvider>>, ttl: Duration) -> Self {
        Self {
            providers,
            cache: Mutex::new(UsageCache::new(ttl)),
        }
    }

    /// The default registry: every provider we support, 60s TTL.
    pub fn with_defaults() -> Self {
        Self::new(
            vec![
                Box::new(codex::CodexProvider::new()),
                Box::new(anthropic::AnthropicProvider::new()),
                Box::new(antigravity::AntigravityProvider::new()),
                Box::new(codebuff::CodebuffProvider::new()),
                Box::new(copilot::CopilotProvider::new()),
                Box::new(cursor::CursorProvider::new()),
                Box::new(doubao::DoubaoProvider::new()),
                Box::new(elevenlabs::ElevenLabsProvider::new()),
                Box::new(factory::FactoryProvider::new()),
                Box::new(gemini::GeminiProvider::new()),
                Box::new(grok::GrokProvider::new()),
                Box::new(jetbrains::JetBrainsProvider::new()),
                Box::new(kimi::KimiProvider::new()),
                Box::new(llmproxy::LlmProxyProvider::new()),
                Box::new(manus::ManusProvider::new()),
                Box::new(mimo::MimoProvider::new()),
                Box::new(minimax::MinimaxProvider::new()),
                Box::new(ollama::OllamaProvider::new()),
                Box::new(opencode::OpenCodeProvider::new()),
                Box::new(opencodego::OpenCodeGoProvider::new()),
                Box::new(stepfun::StepFunProvider::new()),
                Box::new(warp::WarpProvider::new()),
                Box::new(synthetic::SyntheticProvider::new()),
                Box::new(zai::ZaiProvider::new()),
                Box::new(kilo::KiloProvider::new()),
                Box::new(alibaba::AlibabaProvider::new()),
                Box::new(amp::AmpProvider::new()),
            ],
            cache::DEFAULT_TTL,
        )
    }

    /// Provider names registered, for discovery/observability.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Serve `usage.get`: return the usage array, optionally filtered to one
    /// provider. Never fails — a provider error becomes a degraded entry. Serves
    /// from cache when warm.
    pub async fn get_usage(&self, provider_filter: Option<&str>) -> Vec<ProviderUsage> {
        let now = Instant::now();
        if let Some(cached) = self
            .cache
            .lock()
            .expect("usage cache mutex poisoned")
            .get(provider_filter, now)
        {
            return cached;
        }

        let mut out = Vec::new();
        for provider in &self.providers {
            if let Some(filter) = provider_filter {
                if provider.name() != filter {
                    continue;
                }
            }
            let entry = match provider.fetch().await {
                Ok(usage) => usage,
                Err(err) => ProviderUsage::degraded(provider.name(), err),
            };
            out.push(entry);
        }

        self.cache.lock().expect("usage cache mutex poisoned").put(
            provider_filter,
            out.clone(),
            now,
        );
        out
    }

    /// A cheap domain-health snapshot (subc L3): summarize the last cached full
    /// sweep without fetching. Reports how many providers — and how many of the
    /// browser-cookie cohort — were degraded in that sweep, plus the sweep's
    /// age. The serving cache lock being poisoned (a panicked serving task) is
    /// reported as `cache_poisoned`, the one condition the module maps to
    /// `failing`; every other state is `ok` with degraded detail, because a
    /// provider degrading (no local creds) is this prober's normal resting state
    /// on any given box, not a module fault.
    pub fn health(&self) -> HealthSnapshot {
        let cookie_names: std::collections::HashSet<&str> = self
            .providers
            .iter()
            .filter(|p| p.is_cookie_based())
            .map(|p| p.name())
            .collect();
        let providers_total = self.providers.len();
        let cookie_cohort_total = cookie_names.len();

        // Poison-tolerant: a poisoned serving mutex is the real data-path fault
        // we must surface (fail-closed), not panic on in the health path.
        let guard = match self.cache.lock() {
            Ok(guard) => guard,
            Err(_) => return HealthSnapshot::poisoned(providers_total, cookie_cohort_total),
        };
        let sweep = guard.latest_full_sweep(Instant::now());
        drop(guard);

        let Some((entries, age)) = sweep else {
            // No sweep cached yet: healthy, just not-yet-exercised.
            return HealthSnapshot {
                providers_total,
                providers_ok: providers_total,
                degraded: Vec::new(),
                cookie_cohort_total,
                cookie_cohort_degraded: Vec::new(),
                last_sweep_age: None,
                cache_poisoned: false,
            };
        };

        let mut degraded = Vec::new();
        let mut cookie_cohort_degraded = Vec::new();
        for entry in &entries {
            if entry.error.is_some() {
                if cookie_names.contains(entry.provider.as_str()) {
                    cookie_cohort_degraded.push(entry.provider.clone());
                }
                degraded.push(entry.provider.clone());
            }
        }
        let providers_ok = providers_total.saturating_sub(degraded.len());

        HealthSnapshot {
            providers_total,
            providers_ok,
            degraded,
            cookie_cohort_total,
            cookie_cohort_degraded,
            last_sweep_age: Some(age),
            cache_poisoned: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::model::Usage;
    use crate::provider::FetchError;

    /// A stub provider with a controllable outcome, so health() can be tested
    /// against a real sweep rather than a mocked snapshot.
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
        async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
            if self.ok {
                Ok(ProviderUsage::healthy(
                    self.name,
                    None,
                    "test",
                    Usage::default(),
                ))
            } else {
                Err(FetchError::NoSession("stub degraded".into()))
            }
        }
    }

    fn registry(specs: &[(&'static str, bool, bool)]) -> Registry {
        let providers: Vec<Box<dyn UsageProvider>> = specs
            .iter()
            .map(|&(name, cookie, ok)| {
                Box::new(StubProvider { name, cookie, ok }) as Box<dyn UsageProvider>
            })
            .collect();
        Registry::new(providers, cache::DEFAULT_TTL)
    }

    #[tokio::test]
    async fn health_reflects_the_last_sweeps_degraded_providers() {
        let reg = registry(&[
            ("codex", false, true),       // healthy api/oauth provider
            ("cursor", true, false),      // degraded cookie provider (stale login)
            ("amp", true, true),          // healthy cookie provider
            ("elevenlabs", false, false), // degraded api provider (no key)
        ]);

        // Before any sweep: healthy-but-unexercised, no degraded list.
        let pre = reg.health();
        assert_eq!(pre.providers_total, 4);
        assert_eq!(pre.cookie_cohort_total, 2);
        assert_eq!(pre.providers_ok, 4);
        assert!(pre.degraded.is_empty());
        assert_eq!(pre.last_sweep_age, None);
        assert!(!pre.cache_poisoned);

        // Exercise the serving path so a real sweep is cached.
        let _ = reg.get_usage(None).await;

        // Non-vacuity: health() must now reflect the ACTUAL sweep outcome. This
        // assertion fails if health() returns a blind "ok".
        let post = reg.health();
        assert_eq!(post.providers_total, 4);
        assert_eq!(post.providers_ok, 2);
        assert_eq!(post.degraded.len(), 2);
        assert!(post.degraded.contains(&"cursor".to_string()));
        assert!(post.degraded.contains(&"elevenlabs".to_string()));
        // Only the cookie provider that degraded counts toward cohort staleness.
        assert_eq!(post.cookie_cohort_total, 2);
        assert_eq!(post.cookie_cohort_degraded, vec!["cursor".to_string()]);
        assert!(post.last_sweep_age.is_some());
        assert!(!post.cache_poisoned);
        assert!(!post.is_failing());
    }

    #[tokio::test]
    async fn health_all_healthy_reports_none_degraded() {
        let reg = registry(&[("codex", false, true), ("amp", true, true)]);
        let _ = reg.get_usage(None).await;
        let h = reg.health();
        assert_eq!(h.providers_ok, 2);
        assert!(h.degraded.is_empty());
        assert!(h.cookie_cohort_degraded.is_empty());
        assert!(!h.is_failing());
    }

    #[test]
    fn poisoned_cache_reports_failing() {
        let reg = registry(&[("codex", false, true), ("cursor", true, true)]);
        // Poison the serving mutex the way a panicked serving task would.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = reg.cache.lock().unwrap();
            panic!("poison the usage cache mutex");
        }));
        let h = reg.health();
        assert!(h.cache_poisoned);
        assert!(h.is_failing());
        // Totals still computed from the provider set even under poison.
        assert_eq!(h.providers_total, 2);
        assert_eq!(h.cookie_cohort_total, 1);
    }
}
