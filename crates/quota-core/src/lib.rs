//! quota-core — provider usage fetchers + RateWindow normalization + TTL cache.
//!
//! Wire-agnostic: this crate knows nothing about subc. It exposes a [`Registry`]
//! that fetches per-provider usage (reusing each provider's own session) and
//! assembles the silent-degrade `/usage` array Alfonso consumes. The subc module
//! (`quota-module`) wraps this behind the `usage.get` route op.

pub mod alibaba;
pub mod anthropic;
pub mod cache;
pub mod codex;
pub mod copilot;
pub mod doubao;
pub mod elevenlabs;
pub mod env;
pub mod gemini;
pub mod http;
pub mod kilo;
pub mod llmproxy;
pub mod model;
pub mod opencode_auth;
pub mod provider;
pub mod synthetic;
pub mod warp;
pub mod zai;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use cache::UsageCache;
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
                Box::new(copilot::CopilotProvider::new()),
                Box::new(doubao::DoubaoProvider::new()),
                Box::new(elevenlabs::ElevenLabsProvider::new()),
                Box::new(gemini::GeminiProvider::new()),
                Box::new(llmproxy::LlmProxyProvider::new()),
                Box::new(warp::WarpProvider::new()),
                Box::new(synthetic::SyntheticProvider::new()),
                Box::new(zai::ZaiProvider::new()),
                Box::new(kilo::KiloProvider::new()),
                Box::new(alibaba::AlibabaProvider::new()),
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

        self.cache
            .lock()
            .expect("usage cache mutex poisoned")
            .put(provider_filter, out.clone(), now);
        out
    }
}
