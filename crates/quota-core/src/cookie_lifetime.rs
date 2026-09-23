//! How long a deposited session cookie keeps working, measured per site.
//!
//! A session cookie in the vault is static: nothing refreshes it, and nothing
//! writes a rotated value back. It works until the site stops accepting it, and
//! when that happens depends on the site -- some keep a session for months, some
//! rotate it on use, some bind it to where it was issued. Nobody had measured it
//! for the providers insula reads, and it decides which of them can live in the
//! vault at all.
//!
//! Once a cookie is deposited, the user's own browser is out of the loop, so the
//! only thing that can kill it is the site. That makes insula's own fetches the
//! measurement: the age of a credential version at its first rejection, after it
//! had been served, is that site's session lifetime for that capture.
//!
//! AGE IS MEASURED FROM FIRST SERVED, NOT FROM DEPOSIT. A scoped vault row
//! carries no timestamp, so the deposit time is not visible here. The first
//! successful read of a version trails the deposit by at most one refresh
//! interval, which is a fine error bar on lifetimes measured in days. For an
//! exact deposit time, the vault's audit log for that credential records it
//! (`ck auth audit`).
//!
//! Only the credential id and version are ever recorded or printed, never a
//! cookie value.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What one completed fetch of a vault cookie tells the lifetime record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LifetimeEvent {
    /// Nothing worth reporting.
    Quiet,
    /// The first rejection of a version that had been served before.
    FirstRejection {
        record_version: Option<u64>,
        age: Duration,
    },
}

#[derive(Debug)]
struct Seen {
    record_version: Option<u64>,
    first_served: Instant,
    rejection_reported: bool,
}

/// First-served time per cookie credential, keyed by the vault credential id.
#[derive(Debug, Default)]
pub(crate) struct CookieLifetimes {
    seen: HashMap<String, Seen>,
}

impl CookieLifetimes {
    /// Record one completed fetch.
    ///
    /// `served` is a successful read; `rejected` is a `credential_rejected`
    /// verdict. A new record version restarts the clock: it is a new capture,
    /// and its lifetime is its own.
    ///
    /// A rejection is reported ONCE per version, and only when that version was
    /// served first. A version rejected on its very first read never worked, so
    /// it says something about the capture rather than about how long the site
    /// keeps a session, and folding it into the table would read as a lifetime
    /// of zero.
    pub(crate) fn observe(
        &mut self,
        credential_id: &str,
        record_version: Option<u64>,
        served: bool,
        rejected: bool,
        now: Instant,
    ) -> LifetimeEvent {
        if served {
            let fresh = self
                .seen
                .get(credential_id)
                .is_none_or(|seen| seen.record_version != record_version);
            if fresh {
                self.seen.insert(
                    credential_id.to_string(),
                    Seen {
                        record_version,
                        first_served: now,
                        rejection_reported: false,
                    },
                );
            }
            return LifetimeEvent::Quiet;
        }
        if !rejected {
            return LifetimeEvent::Quiet;
        }
        match self.seen.get_mut(credential_id) {
            Some(seen) if seen.record_version == record_version && !seen.rejection_reported => {
                seen.rejection_reported = true;
                LifetimeEvent::FirstRejection {
                    record_version,
                    age: now.saturating_duration_since(seen.first_served),
                }
            }
            _ => LifetimeEvent::Quiet,
        }
    }
}

#[cfg(test)]
impl CookieLifetimes {
    /// Whether this credential's first rejection has been reported, or `None`
    /// when the credential has no lifetime record at all. Lets a test that
    /// drives a scheduler turn check what the registry recorded.
    pub(crate) fn rejection_reported(&self, credential_id: &str) -> Option<bool> {
        self.seen
            .get(credential_id)
            .map(|seen| seen.rejection_reported)
    }
}

/// The age in whole hours and minutes, for a log line a person reads.
pub(crate) fn render_age(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "cookie:ollama.com:ufuk";

    #[test]
    fn the_first_rejection_after_a_served_read_reports_its_age() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        assert_eq!(
            lifetimes.observe(ID, Some(3), true, false, start),
            LifetimeEvent::Quiet
        );
        let later = start + Duration::from_secs(5 * 3600 + 7 * 60);
        assert_eq!(
            lifetimes.observe(ID, Some(3), false, true, later),
            LifetimeEvent::FirstRejection {
                record_version: Some(3),
                age: Duration::from_secs(5 * 3600 + 7 * 60),
            }
        );
    }

    /// Age runs from the FIRST served read, not the latest one. A cookie read
    /// every minute for a day and then rejected lived a day, not a minute.
    #[test]
    fn later_served_reads_do_not_restart_the_clock() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        lifetimes.observe(ID, Some(3), true, false, start);
        lifetimes.observe(ID, Some(3), true, false, start + Duration::from_secs(3600));
        let event = lifetimes.observe(ID, Some(3), false, true, start + Duration::from_secs(7200));
        assert_eq!(
            event,
            LifetimeEvent::FirstRejection {
                record_version: Some(3),
                age: Duration::from_secs(7200),
            }
        );
    }

    /// Reported once: a dead cookie is rejected on every tick until someone
    /// re-captures it, and a line per tick would bury the one that matters.
    #[test]
    fn a_rejection_is_reported_once_per_version() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        lifetimes.observe(ID, Some(3), true, false, start);
        lifetimes.observe(ID, Some(3), false, true, start + Duration::from_secs(60));
        assert_eq!(
            lifetimes.observe(ID, Some(3), false, true, start + Duration::from_secs(120)),
            LifetimeEvent::Quiet
        );
    }

    /// A re-capture is a new version with its own lifetime.
    #[test]
    fn a_new_version_restarts_the_clock_and_can_report_again() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        lifetimes.observe(ID, Some(3), true, false, start);
        lifetimes.observe(ID, Some(3), false, true, start + Duration::from_secs(60));
        let recapture = start + Duration::from_secs(600);
        lifetimes.observe(ID, Some(4), true, false, recapture);
        assert_eq!(
            lifetimes.observe(
                ID,
                Some(4),
                false,
                true,
                recapture + Duration::from_secs(90)
            ),
            LifetimeEvent::FirstRejection {
                record_version: Some(4),
                age: Duration::from_secs(90),
            }
        );
    }

    /// A version rejected before it ever served is a bad capture, not a
    /// lifetime; reporting it would put a zero in the table.
    #[test]
    fn a_version_never_served_reports_nothing() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        assert_eq!(
            lifetimes.observe(ID, Some(3), false, true, start),
            LifetimeEvent::Quiet
        );
        lifetimes.observe(ID, Some(3), true, false, start);
        assert_eq!(
            lifetimes.observe(ID, Some(4), false, true, start + Duration::from_secs(60)),
            LifetimeEvent::Quiet,
            "a rejection of a version other than the one served has no baseline"
        );
    }

    /// A transient failure is neither a read nor a rejection.
    #[test]
    fn a_transient_failure_neither_starts_nor_ends_a_lifetime() {
        let mut lifetimes = CookieLifetimes::default();
        let start = Instant::now();
        lifetimes.observe(ID, Some(3), true, false, start);
        assert_eq!(
            lifetimes.observe(ID, Some(3), false, false, start + Duration::from_secs(60)),
            LifetimeEvent::Quiet
        );
        assert!(matches!(
            lifetimes.observe(ID, Some(3), false, true, start + Duration::from_secs(120)),
            LifetimeEvent::FirstRejection { .. }
        ));
    }

    #[test]
    fn ages_render_as_hours_and_minutes() {
        assert_eq!(render_age(Duration::from_secs(0)), "0h00m");
        assert_eq!(
            render_age(Duration::from_secs(5 * 3600 + 7 * 60 + 59)),
            "5h07m"
        );
        assert_eq!(render_age(Duration::from_secs(49 * 3600)), "49h00m");
    }
}
