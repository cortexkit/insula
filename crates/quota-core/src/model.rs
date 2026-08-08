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

/// Drop any window whose percent is not a number, from every entry about to be
/// published.
///
/// `usedPercent` is a plain `f64` on the wire, and a non-finite one serializes
/// as `null`. A consumer decoding into a typed struct then fails on the WHOLE
/// response -- not the offending entry, the entire array -- so one provider
/// computing `0.0 / 0.0` would take every other provider's usage down with it.
///
/// Dropping the window keeps that blast radius at one window. The percent is the
/// load-bearing field, so a window without a usable one has nothing to say; the
/// alternative of substituting a number would state a capacity nobody measured,
/// and zero in particular would read as an idle account.
///
/// Every division that computes a percent guards its denominator today, so this
/// is a backstop rather than a live fix. It is here because those guards are one
/// correct decision per site and this is one decision for all of them, and
/// because the cost of a single missed guard is not a wrong number but a
/// response no typed consumer can read.
///
/// KNOWN CONSEQUENCE, and the reason this is not simply free: a dropped window
/// is indistinguishable on the wire from a window that never existed. Consumers
/// commonly reduce an account to its *most constrained* window, so a silently
/// dropped one biases that reading DOWNWARD -- an account is reported as having
/// more headroom than it has, which is the expensive direction.
///
/// Nothing here can currently reach that state, since a non-finite percent needs
/// a missed denominator guard upstream. The alternative -- an additional wire
/// field marking the entry as assembled from an incomplete window set -- is a
/// change to a published shared type with several consumers, and adding a field
/// that no provider can populate today would be a signal that is almost always
/// absent. Documented for consumers in `docs/consumer-contract.md` instead, and
/// worth building the moment this path becomes reachable.
pub fn drop_uncomputable_windows(entries: &mut [ProviderUsage]) {
    for entry in entries {
        let Some(usage) = entry.usage.as_mut() else {
            continue;
        };
        let Usage {
            primary,
            secondary,
            tertiary,
            extra_rate_windows,
        } = usage;

        for slot in [primary, secondary, tertiary] {
            if slot.as_ref().is_some_and(|w| !w.used_percent.is_finite()) {
                *slot = None;
            }
        }
        if let Some(extras) = extra_rate_windows.as_mut() {
            for extra in extras.iter_mut() {
                if extra
                    .window
                    .as_ref()
                    .is_some_and(|w| !w.used_percent.is_finite())
                {
                    extra.window = None;
                }
            }
        }

        // `raw_used_percent` is optional on the wire, so a non-finite one
        // serializes as null in a field that already admits absence -- it costs
        // the annotation, not the response.
        for window in windows_mut(usage) {
            if window.raw_used_percent.is_some_and(|raw| !raw.is_finite()) {
                window.raw_used_percent = None;
            }
        }
    }
}

/// Cap on any single upstream-derived string published on an entry.
///
/// An order of magnitude above the longest value any provider produces today
/// (37 bytes, an account UUID), so it cannot truncate real data. It is a bound
/// on what an upstream can put on this wire, not a formatting rule.
const MAX_WIRE_STRING_BYTES: usize = 512;

/// Bound the upstream-derived strings on every entry about to be published.
///
/// Several fields are copied from upstream payloads with no length of their own:
/// a model or bucket identifier becomes an extra window's `id` and `title`, and
/// the account labels come from a token or a credential store. Nothing between
/// those sources and the wire caps them.
///
/// The cost of leaving them unbounded is not a large response. The frame layer
/// refuses a body over its maximum, so a sufficiently long string means the
/// whole reply cannot be sent -- every provider's usage lost to one upstream's
/// oversized label. Bounding each string keeps a bad value costing its own
/// field.
///
/// Truncation is announced rather than silent, for the same reason the error
/// text is: a value cut without saying so reads as a complete one, and an
/// identifier that quietly loses its tail can collide with a sibling.
///
/// Announcing the cut is only sufficient because every field bounded here is
/// PROSE OR AN IDENTIFIER. A truncated number would still be a valid number --
/// `36` cut to `3` is a plausible reading, not a visibly damaged one -- so a
/// marker elsewhere in the string would not protect it. The numeric fields on
/// this wire are typed rather than stringly, so they cannot reach this function
/// at all; that is what closes the hazard, not this bound. A future field
/// carrying a number as text would reopen it, and would need dropping whole
/// rather than truncating.
pub fn bound_wire_strings(entries: &mut [ProviderUsage]) {
    fn bound(value: &mut Option<String>) {
        if let Some(text) = value {
            if text.len() > MAX_WIRE_STRING_BYTES {
                *text = crate::text::truncate_for_wire(text, MAX_WIRE_STRING_BYTES);
            }
        }
    }

    for entry in entries {
        bound(&mut entry.account);
        if let Some(info) = entry.account_info.as_mut() {
            bound(&mut info.email);
            bound(&mut info.org_name);
            bound(&mut info.plan_type);
        }
        if let Some(usage) = entry.usage.as_mut() {
            if let Some(extras) = usage.extra_rate_windows.as_mut() {
                for extra in extras.iter_mut() {
                    bound(&mut extra.id);
                    bound(&mut extra.title);
                }
            }
            // Reset timestamps are not all locally formatted. Most providers
            // build one from an epoch, but around a dozen carry the upstream's
            // own string through -- a provider that reports a reset as text
            // rather than a number has nothing between its response and this
            // field.
            for window in windows_mut(usage) {
                bound(&mut window.resets_at);
            }
        }
        // `provider` and `api_provider` are this module's own names and
        // `fetched_at` is formatted here, so neither can carry an upstream's
        // length. `savedResets` timestamps are formatted from parsed values
        // too. `error` is bounded already, at the single point where a failure
        // becomes wire text.
    }
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

    /// A percent that is not a number costs its own window and nothing else.
    ///
    /// `usedPercent` is a plain `f64` on the wire, so a non-finite value
    /// serializes as `null`, and a consumer decoding into a typed struct fails
    /// on the entire array -- every sibling window and every other provider's
    /// entry lost with it.
    #[test]
    fn a_window_whose_percent_is_not_a_number_is_dropped_alone() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut entries = vec![
                ProviderUsage::healthy(
                    "broken",
                    None,
                    "oauth",
                    Usage {
                        primary: Some(window(bad)),
                        secondary: Some(window(40.0)),
                        extra_rate_windows: Some(vec![
                            ExtraWindow {
                                id: Some("bad-extra".into()),
                                title: None,
                                window: Some(window(bad)),
                            },
                            ExtraWindow {
                                id: Some("good-extra".into()),
                                title: None,
                                window: Some(window(55.0)),
                            },
                        ]),
                        ..Usage::default()
                    },
                ),
                ProviderUsage::healthy(
                    "healthy",
                    None,
                    "oauth",
                    Usage {
                        primary: Some(window(10.0)),
                        ..Usage::default()
                    },
                ),
            ];

            drop_uncomputable_windows(&mut entries);

            let broken = entries[0].usage.as_ref().unwrap();
            assert!(broken.primary.is_none(), "{bad} was published");
            assert!(broken.extra_rate_windows.as_ref().unwrap()[0]
                .window
                .is_none());

            // Not vacuous: only the unusable windows go, so this cannot pass by
            // discarding the entry.
            assert_eq!(broken.secondary.as_ref().unwrap().used_percent, 40.0);
            assert_eq!(
                broken.extra_rate_windows.as_ref().unwrap()[1]
                    .window
                    .as_ref()
                    .unwrap()
                    .used_percent,
                55.0
            );
            assert_eq!(
                entries[1]
                    .usage
                    .as_ref()
                    .unwrap()
                    .primary
                    .as_ref()
                    .unwrap()
                    .used_percent,
                10.0,
                "a sibling entry lost its usage"
            );

            // The published bytes are what matter: no null reaches the wire.
            let json = serde_json::to_string(&entries).unwrap();
            assert!(!json.contains("null"), "a null reached the wire: {json}");
        }
    }

    /// An unusable relaxed annotation costs the annotation, not the window.
    ///
    /// `rawUsedPercent` is optional on the wire, so its absence is a state
    /// consumers already handle -- unlike `usedPercent`, dropping it is cheap.
    #[test]
    fn an_unusable_raw_percent_costs_the_annotation_not_the_window() {
        let mut bad_raw = window(30.0);
        bad_raw.raw_used_percent = Some(f64::NAN);
        let mut entries = vec![ProviderUsage::healthy(
            "codex",
            None,
            "oauth",
            Usage {
                primary: Some(bad_raw),
                ..Usage::default()
            },
        )];

        drop_uncomputable_windows(&mut entries);

        let primary = entries[0]
            .usage
            .as_ref()
            .unwrap()
            .primary
            .as_ref()
            .expect("the window survives");
        assert_eq!(primary.raw_used_percent, None);
        assert_eq!(primary.used_percent, 30.0);
    }

    /// An upstream string long enough to break the reply is cut to its own
    /// field.
    ///
    /// A model or bucket identifier becomes an extra window's `id` and `title`
    /// with no length of its own, and the account labels come from a token or a
    /// credential store. The frame layer refuses a body over its maximum, so
    /// without a bound one upstream's oversized label costs every provider's
    /// usage: the reply cannot be sent at all.
    #[test]
    fn an_oversized_upstream_string_is_cut_to_its_own_field() {
        let huge = "A".repeat(4096);
        let mut entries = vec![ProviderUsage::healthy(
            "antigravity",
            Some(huge.clone()),
            "oauth",
            Usage {
                primary: Some(window(10.0)),
                extra_rate_windows: Some(vec![ExtraWindow {
                    id: Some(huge.clone()),
                    title: Some(huge.clone()),
                    window: Some(window(20.0)),
                }]),
                ..Usage::default()
            },
        )];
        entries[0].account_info = Some(AccountInfo {
            email: Some(huge.clone()),
            org_name: Some(huge.clone()),
            plan_type: Some(huge),
        });

        bound_wire_strings(&mut entries);

        let entry = &entries[0];
        let info = entry.account_info.as_ref().unwrap();
        let usage = entry.usage.as_ref().unwrap();
        let extra = &usage.extra_rate_windows.as_ref().unwrap()[0];
        for (name, value) in [
            ("account", entry.account.as_deref()),
            ("email", info.email.as_deref()),
            ("orgName", info.org_name.as_deref()),
            ("planType", info.plan_type.as_deref()),
            ("extra.id", extra.id.as_deref()),
            ("extra.title", extra.title.as_deref()),
        ] {
            let value = value.expect("the field survives");
            assert!(
                value.len() <= 600,
                "{name} was published at {} bytes",
                value.len()
            );
            // Announced rather than silent: a value cut without saying so reads
            // as a complete one, and an identifier that quietly loses its tail
            // can collide with a sibling.
            assert!(value.contains("more bytes]"), "{name} was cut silently");
        }

        // Not vacuous: the measurements are untouched, so this cannot pass by
        // discarding the entry.
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, 10.0);
        assert_eq!(extra.window.as_ref().unwrap().used_percent, 20.0);
    }

    /// Ordinary values pass through byte-identical, so the bound cannot pass by
    /// rewriting everything it touches.
    /// A reset timestamp is bounded like any other upstream string.
    ///
    /// Most providers build this field from an epoch number, so it cannot carry
    /// an upstream's length -- but around a dozen pass the upstream's own text
    /// through, and for those there is nothing between the response and the
    /// wire. The longest real one observed is 32 bytes.
    #[test]
    fn an_oversized_reset_timestamp_is_cut_like_any_other_string() {
        let huge = "9".repeat(4096);
        let mut slot = window(10.0);
        slot.resets_at = Some(huge.clone());
        let mut extra_window = window(20.0);
        extra_window.resets_at = Some(huge.clone());

        let mut entries = vec![ProviderUsage::healthy(
            "anthropic",
            None,
            "oauth",
            Usage {
                primary: Some(slot),
                extra_rate_windows: Some(vec![ExtraWindow {
                    id: Some("weekly".into()),
                    title: None,
                    window: Some(extra_window),
                }]),
                ..Usage::default()
            },
        )];

        bound_wire_strings(&mut entries);

        let usage = entries[0].usage.as_ref().unwrap();
        for (label, window) in [
            ("primary", usage.primary.as_ref().unwrap()),
            (
                "extra",
                usage.extra_rate_windows.as_ref().unwrap()[0]
                    .window
                    .as_ref()
                    .unwrap(),
            ),
        ] {
            let reset = window.resets_at.as_deref().expect("the field survives");
            assert!(
                reset.len() <= MAX_WIRE_STRING_BYTES + 32,
                "{label} reset published {} bytes",
                reset.len()
            );
            // Not vacuous: the cut is announced, so a truncated timestamp
            // cannot be mistaken for a complete one.
            assert!(reset.contains("more bytes]"), "{label}: {reset}");
            // And the rest of the window is untouched.
            assert_eq!(
                window.used_percent,
                if label == "primary" { 10.0 } else { 20.0 }
            );
        }
    }

    #[test]
    fn an_ordinary_string_is_published_unchanged() {
        let mut entries = vec![ProviderUsage::healthy(
            "codex",
            Some("291f5165-0a65-4635-b437-5174415ed928".into()),
            "vault",
            Usage {
                primary: Some(window(10.0)),
                extra_rate_windows: Some(vec![ExtraWindow {
                    id: Some("gemini-2.5-flash-lite".into()),
                    title: Some("gemini-2.5-flash-lite".into()),
                    window: Some(window(20.0)),
                }]),
                ..Usage::default()
            },
        )];
        let before = serde_json::to_string(&entries).unwrap();

        bound_wire_strings(&mut entries);

        assert_eq!(serde_json::to_string(&entries).unwrap(), before);
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
