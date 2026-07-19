//! The usage output model — re-exported from the shared `cortexkit-provider-usage`
//! crate so the quota module and every wire consumer (ALF's router, astrocyte's
//! capacity axis, the `ck quota` renderer) compile against one definition of the
//! `usage.get` payload shape.
//!
//! The wire contract these types implement (camelCase keys, error-skipping,
//! optional `resetsAt`, the banked-reset relaxation fields) is documented on the
//! shared crate. Read-time transform behavior — the relaxation that zeroes
//! `used_percent` and populates `raw_used_percent` — lives in this crate's
//! registry (`lib.rs`), NOT in the types: the shared crate is shape, not policy.
//!
//! NOTE: the reserved prepaid-`Balance` seam was removed when the types moved to
//! the shared crate (it was never populated by any provider and is wire-neutral:
//! the field was always `None` and skipped). It returns to the shared crate,
//! additively, when the balance axis is actually designed.

pub use cortexkit_provider_usage::{
    AccountInfo, CreditExpiry, ExtraWindow, ProviderUsage, RateWindow, SavedResets, Usage,
};
