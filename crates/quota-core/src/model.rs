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

/// Every rate window a `Usage` carries, in slot order then extras.
///
/// The slots are destructured rather than named field by field, so a slot added
/// to the shared wire type breaks this build instead of being silently skipped.
/// That failure is quiet and one-directional wherever it matters: a caller
/// summing usage sees *less* than the account has, so a walled account can read
/// as having room.
///
/// Use this anywhere a decision depends on all of an account's windows. Parsers
/// building a `Usage` from one provider's payload do not need it — they know
/// which slots they populate, and a compile error there would be noise.
pub fn windows(usage: &Usage) -> impl Iterator<Item = &RateWindow> {
    let Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows,
    } = usage;

    [primary.as_ref(), secondary.as_ref(), tertiary.as_ref()]
        .into_iter()
        .flatten()
        .chain(
            extra_rate_windows
                .iter()
                .flat_map(|windows| windows.iter())
                .filter_map(|extra| extra.window.as_ref()),
        )
}

/// [`windows`], mutably: the same enumeration for read-time transforms.
pub fn windows_mut(usage: &mut Usage) -> impl Iterator<Item = &mut RateWindow> {
    let Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows,
    } = usage;

    [primary.as_mut(), secondary.as_mut(), tertiary.as_mut()]
        .into_iter()
        .flatten()
        .chain(
            extra_rate_windows
                .iter_mut()
                .flat_map(|windows| windows.iter_mut())
                .filter_map(|extra| extra.window.as_mut()),
        )
}
