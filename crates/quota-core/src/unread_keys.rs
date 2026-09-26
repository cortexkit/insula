//! Say so when a provider's usage response carries keys insula does not read.
//!
//! WHY. A provider that adds a field to its usage response is not an error: the
//! typed struct simply skips it, and the addition goes unnoticed until someone
//! happens to read a raw payload by hand. Two real gaps (Anthropic's overage
//! block and Codex's credit limit) were found exactly that way. This module turns
//! the silence into one stderr line naming what was skipped.
//!
//! The set of "known" keys is never written down here. `serde_ignored` reports
//! every path serde skipped while filling the typed struct, so the struct itself
//! is the list and cannot drift from a copy of it.
//!
//! What it deliberately does NOT do:
//! - It never reports a VALUE, only key names, because a usage payload carries
//!   account identifiers and balances that do not belong in a log.
//! - It is not a health metric. Anthropic alone sends about twenty top-level keys
//!   insula has no use for, so a count would sit permanently above zero and stop
//!   being read. A line naming the keys, once, is actionable; a gauge is not.
//! - It does not change any decode result. [`decode_reporting_unread`] returns
//!   exactly what `serde_json::from_slice` would, success or error, so no
//!   provider's classification of a response moves because of it.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde::de::DeserializeOwned;

use crate::LOG_TAG;

/// Longest key name printed, in characters. Key names come from an upstream
/// server, so one could be arbitrarily long; a line that size is a log problem.
const MAX_NAME_CHARS: usize = 64;

/// Most names printed on one line; the rest are counted as `+N more`. The ones
/// not printed are not remembered either, so a later response names them.
const MAX_NAMES_PER_LINE: usize = 20;

/// Most names remembered per provider. The memory exists so each name is said
/// once per process; without a ceiling, a server inventing fresh key names on
/// every poll would grow it for as long as the process lives. Past the ceiling
/// the provider simply stops being reported on.
const MAX_REMEMBERED_PER_PROVIDER: usize = 512;

/// How many segments of a path are kept: the top level and one level below.
/// A deeper unread key is reported by its first two segments followed by `.*`,
/// which points a reader at the right object without claiming the object itself
/// is unread.
const MAX_SEGMENTS: usize = 2;

/// Names already reported in this process, per provider.
fn reported() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static REPORTED: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    REPORTED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decode `body` into `T` exactly as `serde_json::from_slice` would, and print
/// one line naming any keys the struct skipped that have not already been
/// reported for `provider` in this process.
pub fn decode_reporting_unread<T: DeserializeOwned>(
    provider: &str,
    body: &[u8],
) -> Result<T, serde_json::Error> {
    let (result, unread) = decode_collecting_unread::<T>(body);
    if result.is_ok() {
        report_unread(provider, unread);
    }
    result
}

/// Decode like `serde_json::from_slice` and also return the skipped key names,
/// already shortened, sanitized and deduplicated, without reporting them.
///
/// For a caller that only knows after decoding whether the result is the one it
/// will use (a decode that may fall back to a second shape), and so must not
/// report keys for a shape it is about to discard.
pub(crate) fn decode_collecting_unread<T: DeserializeOwned>(
    body: &[u8],
) -> (Result<T, serde_json::Error>, Vec<String>) {
    let mut unread: Vec<String> = Vec::new();
    // Mirrors `serde_json::from_slice`: deserialize, then `end()` to refuse
    // trailing characters. Leaving `end()` out would accept `{} junk`, which the
    // plain decode rejects.
    let mut de = serde_json::Deserializer::from_slice(body);
    let result = serde_ignored::deserialize(&mut de, |path| {
        let name = render_path(&path);
        if !unread.contains(&name) {
            unread.push(name);
        }
    })
    .and_then(|value| de.end().map(|()| value));
    (result, unread)
}

/// Print the names in `unread` not yet reported for `provider`, and return
/// exactly the names that were printed (for tests; production ignores it).
pub(crate) fn report_unread(provider: &str, unread: Vec<String>) -> Vec<String> {
    let (fresh, more) = remember_new(provider, unread);
    if let Some(line) = format_line(provider, &fresh, more) {
        eprintln!("{line}");
    }
    fresh
}

/// Split `unread` into the names to print now (recorded as reported) and a
/// count of further new names held back by the per-line cap.
fn remember_new(provider: &str, unread: Vec<String>) -> (Vec<String>, usize) {
    let mut reported = reported()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let seen = reported.entry(provider.to_string()).or_default();
    let mut fresh = Vec::new();
    let mut more = 0;
    for name in unread {
        if seen.contains(&name) || fresh.contains(&name) {
            continue;
        }
        if fresh.len() >= MAX_NAMES_PER_LINE || seen.len() >= MAX_REMEMBERED_PER_PROVIDER {
            more += 1;
            continue;
        }
        seen.insert(name.clone());
        fresh.push(name);
    }
    (fresh, more)
}

/// Whether `name` has been reported for `provider` in this process. Lets a
/// provider's own tests prove its decode is wired through this module.
#[cfg(test)]
pub(crate) fn was_reported(provider: &str, name: &str) -> bool {
    let reported = reported()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    reported
        .get(provider)
        .is_some_and(|seen| seen.contains(name))
}

/// The one stderr line, or `None` when there is nothing new to say.
fn format_line(provider: &str, names: &[String], more: usize) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let mut line = format!(
        "{LOG_TAG} {provider}: response carries keys insula does not read: {}",
        names.join(", ")
    );
    if more > 0 {
        line.push_str(&format!(" +{more} more"));
    }
    Some(line)
}

/// Render a skipped path as at most two dotted segments, with array indices
/// collapsed to `[]` so every element of a list reports as one name.
fn render_path(path: &serde_ignored::Path<'_>) -> String {
    let mut segments: Vec<String> = Vec::new();
    collect_segments(path, &mut segments);
    let deeper = segments.len() > MAX_SEGMENTS;
    segments.truncate(MAX_SEGMENTS);
    let mut name = segments.join(".");
    if deeper {
        name.push_str(".*");
    }
    sanitize(&name)
}

/// Walk from the root to `path`, producing one segment per object key. A list
/// index is not a segment of its own: it marks the key holding the list with
/// `[]`, so `limits.3.bar` becomes `limits[]` then `bar`.
fn collect_segments(path: &serde_ignored::Path<'_>, segments: &mut Vec<String>) {
    use serde_ignored::Path;
    match path {
        Path::Root => {}
        Path::Seq { parent, .. } => {
            collect_segments(parent, segments);
            match segments.last_mut() {
                Some(last) => last.push_str("[]"),
                // A list at the very top of the body.
                None => segments.push("[]".to_string()),
            }
        }
        Path::Map { parent, key } => {
            collect_segments(parent, segments);
            segments.push(key.clone());
        }
        // Option and newtype wrappers are the struct's shape, not the payload's.
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => collect_segments(parent, segments),
    }
}

/// Make an upstream-supplied name safe to print: control characters (a newline
/// could forge a second log line) become `?`, and the length is capped.
fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(MAX_NAME_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if name.chars().count() > MAX_NAME_CHARS {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Inner {
        used: Option<f64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Body {
        five_hour: Option<Inner>,
        limits: Option<Vec<Inner>>,
        nested: Option<Outer>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Outer {
        inner: Option<Inner>,
    }

    /// Decode and report under `provider`, returning the names that were new.
    fn reported_names(provider: &str, body: &str) -> Vec<String> {
        let (result, unread) = decode_collecting_unread::<Body>(body.as_bytes());
        result.expect("test body must decode");
        report_unread(provider, unread)
    }

    #[test]
    fn unknown_top_level_and_nested_keys_are_reported_and_read_keys_are_not() {
        let names = reported_names(
            "test-unread-basic",
            r#"{"five_hour": {"used": 1, "new_inner": 2}, "seven_day_foo": {}}"#,
        );
        assert_eq!(names, vec!["five_hour.new_inner", "seven_day_foo"]);
    }

    #[test]
    fn a_second_decode_of_the_same_body_reports_nothing() {
        let body = r#"{"five_hour": {"used": 1}, "overage": 3}"#;
        assert_eq!(reported_names("test-unread-once", body), vec!["overage"]);
        assert!(reported_names("test-unread-once", body).is_empty());
    }

    #[test]
    fn depth_is_capped_at_two_segments() {
        let names = reported_names(
            "test-unread-depth",
            r#"{"nested": {"inner": {"used": 1, "third": {"fourth": 1}}}}"#,
        );
        // serde_ignored names a skipped object, not its contents, so a path
        // only runs this deep when the objects above the key are ones the
        // struct reads.
        assert_eq!(names, vec!["nested.inner.*"]);
    }

    #[test]
    fn array_indices_collapse() {
        let names = reported_names(
            "test-unread-array",
            r#"{"limits": [{"used": 1, "bar": 1}, {"used": 2, "bar": 2}, {"baz": 3}]}"#,
        );
        assert_eq!(names, vec!["limits[].bar", "limits[].baz"]);
    }

    #[test]
    fn decode_results_match_from_slice_exactly() {
        let bodies: &[&str] = &[
            r#"{"five_hour": {"used": 1}, "extra": true}"#,
            r#"{"five_hour": {"used": "not a number"}}"#,
            r#"{"five_hour": "#,
            r#"{"five_hour": null} trailing"#,
            "not json",
            "",
        ];
        for body in bodies {
            let plain = serde_json::from_slice::<Body>(body.as_bytes());
            let (reporting, _) = decode_collecting_unread::<Body>(body.as_bytes());
            match (plain, reporting) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "body {body:?}"),
                (Err(a), Err(b)) => {
                    assert_eq!(a.to_string(), b.to_string(), "body {body:?}");
                    assert_eq!(a.classify(), b.classify(), "body {body:?}");
                }
                (a, b) => panic!("body {body:?}: from_slice {a:?}, reporting {b:?}"),
            }
        }
    }

    #[test]
    fn a_failed_decode_reports_nothing() {
        let body = br#"{"five_hour": {"used": "x"}, "leaked_key": 1}"#;
        assert!(decode_reporting_unread::<Body>("test-unread-failed", body).is_err());
        // Had the failed decode reported, this name would already be spent.
        assert_eq!(
            reported_names("test-unread-failed", r#"{"leaked_key": 1}"#),
            vec!["leaked_key"]
        );
    }

    #[test]
    fn no_value_ever_appears_in_the_line() {
        let secret = "sk-ant-oat01-SECRETVALUE";
        let body = format!(
            r#"{{"access_token": "{secret}", "five_hour": {{"used": 1, "token": "{secret}"}}, "limits": [{{"key": "{secret}"}}]}}"#
        );
        let names = reported_names("test-unread-secret", &body);
        assert_eq!(
            names,
            vec!["access_token", "five_hour.token", "limits[].key"]
        );
        let line = format_line("test-unread-secret", &names, 0).expect("names were reported");
        assert!(!line.contains("SECRETVALUE"), "{line}");
        assert!(line.starts_with("[insula] test-unread-secret: "), "{line}");
    }

    #[test]
    fn names_are_sanitized_and_capped() {
        let long = "k".repeat(200);
        let body = format!(r#"{{"evil\nname": 1, "{long}": 2}}"#);
        let names = reported_names("test-unread-sanitize", &body);
        assert_eq!(names[0], "evil?name");
        assert_eq!(names[1], format!("{}...", "k".repeat(MAX_NAME_CHARS)));
    }

    #[test]
    fn names_past_the_per_line_cap_are_counted_and_reported_later() {
        let keys: Vec<String> = (0..25).map(|i| format!(r#""extra{i:02}": 1"#)).collect();
        let body = format!("{{{}}}", keys.join(", "));
        let (result, unread) = decode_collecting_unread::<Body>(body.as_bytes());
        result.unwrap();
        let (first, more) = remember_new("test-unread-cap", unread.clone());
        assert_eq!(first.len(), MAX_NAMES_PER_LINE);
        assert_eq!(more, 5);
        let line = format_line("test-unread-cap", &first, more).unwrap();
        assert!(line.ends_with(" +5 more"), "{line}");
        let (second, more) = remember_new("test-unread-cap", unread);
        assert_eq!(second.len(), 5);
        assert_eq!(more, 0);
    }
}
