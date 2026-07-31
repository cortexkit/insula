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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used_percent: f64) -> RateWindow {
        RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at: None,
            window_minutes: None,
            used_count: None,
            total_count: None,
        }
    }

    /// A filled slot after an empty one must still be reached.
    ///
    /// The slots are independent positions rather than a contiguous list, and
    /// upstreams report each of their windows separately: one provider fills
    /// `tertiary` from a field that can arrive while the field behind
    /// `secondary` is absent, so a hole is a shape this module really emits.
    ///
    /// This matters because these helpers feed decisions rather than displays.
    /// One decides whether an account has hit its wall, which spends a banked
    /// reset credit; the other rewrites what consumers pace on. Both read the
    /// account's worst window, so a window skipped here reads as *less* usage
    /// than the account has -- a walled account looking like it has room.
    #[test]
    fn a_gap_between_slots_does_not_stop_the_walk() {
        let usage = Usage {
            primary: Some(window(10.0)),
            secondary: None,
            tertiary: Some(window(99.0)),
            extra_rate_windows: None,
        };

        let percents: Vec<f64> = windows(&usage).map(|w| w.used_percent).collect();

        // Not vacuous: the 99 is the whole point. An implementation that stopped
        // at the empty slot would collect only [10.0] and report an account at
        // 10% when it is at 99%.
        assert_eq!(percents, vec![10.0, 99.0]);
    }

    #[test]
    fn extra_windows_are_walked_after_the_slots() {
        let usage = Usage {
            primary: Some(window(1.0)),
            secondary: None,
            tertiary: None,
            extra_rate_windows: Some(vec![
                ExtraWindow {
                    title: Some("named".into()),
                    id: Some("named".into()),
                    window: Some(window(50.0)),
                },
                // An entry naming a limit whose figure could not be read. It
                // must not end the walk: the entries after it are real.
                ExtraWindow {
                    title: Some("unreadable".into()),
                    id: Some("unreadable".into()),
                    window: None,
                },
                ExtraWindow {
                    title: Some("last".into()),
                    id: Some("last".into()),
                    window: Some(window(88.0)),
                },
            ]),
        };

        let percents: Vec<f64> = windows(&usage).map(|w| w.used_percent).collect();
        assert_eq!(percents, vec![1.0, 50.0, 88.0]);
    }

    /// The mutable walk must reach exactly the same windows as the shared one.
    ///
    /// They are separate implementations of one rule, so nothing but a test
    /// keeps them in step: a window the read-time transform misses would be
    /// published with the provider's raw percent as its effective one.
    #[test]
    fn the_mutable_walk_reaches_the_same_windows() {
        let mut usage = Usage {
            primary: Some(window(10.0)),
            secondary: None,
            tertiary: Some(window(99.0)),
            extra_rate_windows: Some(vec![
                ExtraWindow {
                    title: None,
                    id: None,
                    window: Some(window(7.0)),
                },
                ExtraWindow {
                    title: None,
                    id: None,
                    window: None,
                },
            ]),
        };

        let read: Vec<f64> = windows(&usage).map(|w| w.used_percent).collect();
        let written: Vec<f64> = windows_mut(&mut usage).map(|w| w.used_percent).collect();
        assert_eq!(read, written);
        assert_eq!(read, vec![10.0, 99.0, 7.0]);
    }

    #[test]
    fn an_empty_usage_walks_nothing() {
        let usage = Usage::default();
        assert_eq!(windows(&usage).count(), 0);
    }
}
