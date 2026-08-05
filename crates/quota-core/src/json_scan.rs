//! Reading a value from a JSON object that may name it several ways.
//!
//! Upstreams that publish the same figure under different keys across versions
//! or plans are read by scanning a list of aliases. The scan is shared rather
//! than written per provider because it carries a rule with consequences: a
//! value that cannot be a percent or a count must be *skipped* so the scan
//! continues to the next alias, not returned.
//!
//! Two copies of this scan existed, and they had already diverged -- one
//! rejected a non-finite value, the other returned it. That is the shape where
//! a fix lands in one copy and the rest stay silently wrong, so the rule lives
//! in one place now.

use serde_json::{Map, Value};

/// The first alias holding a usable number, as a float.
///
/// A value is usable when it is finite. A non-finite one is skipped rather than
/// returned, and the distinction matters because this is a *scan*: returning it
/// stops the search at a garbage alias, so a valid figure under a later alias is
/// never reached. The account then publishes no window at all -- which a
/// consumer cannot tell from capacity nobody measured -- where it could have
/// published the real number.
///
/// Reachable only through the string branch. A JSON *number* cannot be
/// non-finite: `serde_json` rejects a bare `NaN` or `Infinity` token, so the
/// only route is a quoted `"NaN"` or `"inf"`, which `f64::from_str` accepts.
pub(crate) fn first_finite_f64(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let Some(value) = map.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_f64() {
            if number.is_finite() {
                return Some(number);
            }
            continue;
        }
        if let Some(text) = value.as_str() {
            if let Ok(number) = text.trim().parse::<f64>() {
                if number.is_finite() {
                    return Some(number);
                }
            }
        }
    }
    None
}

/// The first alias holding a usable integer.
///
/// No finiteness check is needed or possible: an integer parse rejects `"NaN"`
/// and `"inf"` outright, so an unusable value already fails to parse and the
/// scan moves on.
pub(crate) fn first_i64(map: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    for key in keys {
        let Some(value) = map.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_i64() {
            return Some(number);
        }
        if let Some(text) = value.as_str() {
            if let Ok(number) = text.trim().parse::<i64>() {
                return Some(number);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(raw: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(raw)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    /// A garbage alias must not stop the scan.
    ///
    /// The failure this prevents is not a wrong number but a missing window:
    /// returning the unusable value makes it the account's percent, which is
    /// dropped before publication, so the account reports no window at all --
    /// indistinguishable from capacity nobody measured. Skipping it finds the
    /// figure the upstream actually published.
    #[test]
    fn an_unusable_alias_is_skipped_so_a_later_one_is_found() {
        let keys = ["usedPercent", "used_percent", "percentUsed"];

        for garbage in [r#""NaN""#, r#""inf""#, r#""-inf""#] {
            let object = map(&format!(
                r#"{{ "usedPercent": {garbage}, "percentUsed": 73.5 }}"#
            ));
            assert_eq!(
                first_finite_f64(&object, &keys),
                Some(73.5),
                "{garbage} stopped the scan"
            );
        }

        // Not vacuous: the scan still returns the first alias when it is usable,
        // so this cannot pass by always preferring a later key.
        let object = map(r#"{ "usedPercent": 12.5, "percentUsed": 73.5 }"#);
        assert_eq!(first_finite_f64(&object, &keys), Some(12.5));
    }

    /// With no usable alias the answer is absence, not a garbage value.
    #[test]
    fn only_unusable_aliases_yield_nothing() {
        let object = map(r#"{ "usedPercent": "NaN" }"#);
        assert_eq!(first_finite_f64(&object, &["usedPercent"]), None);
    }

    /// Both scans read a number written as a string, which upstreams do.
    #[test]
    fn a_quoted_number_is_read_like_a_bare_one() {
        let object = map(r#"{ "limit": " 40000 ", "percent": " 12.5 " }"#);
        assert_eq!(first_i64(&object, &["limit"]), Some(40_000));
        assert_eq!(first_finite_f64(&object, &["percent"]), Some(12.5));
    }

    /// An integer scan cannot yield a non-finite value, so it needs no guard.
    #[test]
    fn an_integer_scan_rejects_the_values_that_need_guarding_elsewhere() {
        let object = map(r#"{ "count": "NaN", "other": 7 }"#);
        assert_eq!(first_i64(&object, &["count", "other"]), Some(7));
    }
}
