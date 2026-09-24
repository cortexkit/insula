//! One-shot diagnostic: feed the REAL scoped id set through the REAL mapper.
//!
//! Written after the cutover took lanes dark on deploy and three hypotheses --
//! routing table, grant category, retain-after-failure -- were each eliminated by
//! measurement without finding the cause. Reading source had stopped paying; this
//! runs the production mapper over the exact 17 ids the live grant returns and
//! prints which providers get a handle and which do not.
//!
//! Offline by construction: no vault, no daemon, no network. The ids are public
//! identifiers, not secrets.

use quota_core::credential_source::{ScopedRowState, ScopedSnapshot};
use quota_core::vault_handles::VaultHandleLoader;
use std::time::Instant;

fn row(id: &str, kind: &str) -> ScopedRowState {
    ScopedRowState {
        credential_id: id.to_string(),
        credential_type: kind.to_string(),
        record_version: 1,
        state: "active".to_string(),
        account_id: None,
    }
}

fn main() {
    let rows = vec![
        row("antigravity:google", "oauth"),
        row("apikey:cerebras", "apikey"),
        row("apikey:deepseek", "apikey"),
        row("apikey:fireworks-ai", "apikey"),
        row("apikey:kimi-for-coding", "apikey"),
        row("apikey:openai:astro", "apikey"),
        row("apikey:openai", "apikey"),
        row("apikey:openrouter", "apikey"),
        row("chatgpt:openai:gmail", "oauth"),
        row("chatgpt:openai", "oauth"),
        row("oauth:anthropic:ufuk2", "oauth"),
        row("oauth:anthropic:umutaday", "oauth"),
        row("oauth:anthropic:wwaxgmail", "oauth"),
        row("oauth:anthropic:yiyi", "oauth"),
        row("oauth:anthropic", "oauth"),
        row("oauth:cursor", "oauth"),
        row("oauth:xai", "oauth"),
    ];
    println!("  rows in: {}", rows.len());

    let loader = VaultHandleLoader::new(None);
    let installed = loader.install_snapshot(ScopedSnapshot { grants: 1, rows }, Instant::now());
    println!("  authoritative: {}", installed.authoritative);
    if let Some(warning) = loader.warning() {
        println!("  mapping warning: {warning}");
    }

    match loader.codex_handles() {
        Ok(h) if h.is_empty() => println!("  codex            NONE"),
        Ok(h) => println!(
            "  codex            {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  codex            ERROR {e:?}"),
    }
    match loader.anthropic_handles() {
        Ok(h) if h.is_empty() => println!("  anthropic        NONE"),
        Ok(h) => println!(
            "  anthropic        {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  anthropic        ERROR {e:?}"),
    }
    match loader.grok_handles() {
        Ok(h) if h.is_empty() => println!("  grok             NONE"),
        Ok(h) => println!(
            "  grok             {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  grok             ERROR {e:?}"),
    }
    match loader.gemini_handles() {
        Ok(h) if h.is_empty() => println!("  gemini           NONE"),
        Ok(h) => println!(
            "  gemini           {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  gemini           ERROR {e:?}"),
    }
    match loader.antigravity_handles() {
        Ok(h) if h.is_empty() => println!("  antigravity      NONE"),
        Ok(h) => println!(
            "  antigravity      {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  antigravity      ERROR {e:?}"),
    }
    match loader.kimi_for_coding_handles() {
        Ok(h) if h.is_empty() => println!("  kimi_for_coding  NONE"),
        Ok(h) => println!(
            "  kimi_for_coding  {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  kimi_for_coding  ERROR {e:?}"),
    }
    match loader.cursor_handles() {
        Ok(h) if h.is_empty() => println!("  cursor           NONE"),
        Ok(h) => println!(
            "  cursor           {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  cursor           ERROR {e:?}"),
    }
    match loader.deepseek_handles() {
        Ok(h) if h.is_empty() => println!("  deepseek         NONE"),
        Ok(h) => println!(
            "  deepseek         {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  deepseek         ERROR {e:?}"),
    }
    match loader.synthetic_handles() {
        Ok(h) if h.is_empty() => println!("  synthetic        NONE"),
        Ok(h) => println!(
            "  synthetic        {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  synthetic        ERROR {e:?}"),
    }
    match loader.openrouter_handles() {
        Ok(h) if h.is_empty() => println!("  openrouter       NONE"),
        Ok(h) => println!(
            "  openrouter       {}",
            h.iter()
                .map(|x| x.stable_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("  openrouter       ERROR {e:?}"),
    }
}
