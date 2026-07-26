//! Small text helpers shared by the HTML/text-scraping providers.

/// Round `index` down to the nearest UTF-8 character boundary in `text`.
///
/// Slicing a `&str` at an arbitrary byte offset panics when the offset lands
/// inside a multibyte character, and any provider response can carry
/// user-chosen text. Scrapers that bound a search window by a fixed BYTE budget
/// (rather than by a match position, which is always a boundary) must pass that
/// budget through here before slicing.
///
/// Returns `text.len()` when `index` is past the end, so callers can clamp and
/// round in one step.
pub(crate) fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut end = index;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Trim `raw`, strip one matching pair of surrounding ASCII quotes, and trim
/// again. Returns `None` when nothing is left.
///
/// Every provider that reads a credential from an environment variable or a
/// settings file needs this, because a value pasted into a shell profile or a
/// JSON blob routinely arrives wrapped in quotes.
///
/// The subtlety is why this is shared rather than repeated: a one-character
/// input of `"` satisfies BOTH `starts_with('"')` and `ends_with('"')` — the
/// same character answers both — so the obvious strip computes `value[1..0]`
/// and panics on a backwards range. A panicking fetch is classified
/// non-transient, which clears the provider's cached window and suppresses it
/// for the backoff, so a stray quote in a config file reads downstream as a
/// provider that has stopped existing rather than one that is misconfigured.
pub(crate) fn strip_wrapping_quotes(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // A single quote character is its own opening AND closing quote, so the pair
    // must be two DISTINCT characters before anything is stripped.
    let unwrapped = if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let cleaned = unwrapped.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_quote_character_is_not_treated_as_a_wrapping_pair() {
        // The regression this helper exists for: `"` starts with a quote and ends
        // with a quote because it IS one character, so an unguarded strip slices
        // [1..0] and panics.
        assert_eq!(strip_wrapping_quotes("\""), Some("\"".to_string()));
        assert_eq!(strip_wrapping_quotes("'"), Some("'".to_string()));
        assert_eq!(strip_wrapping_quotes("  \"  "), Some("\"".to_string()));
    }

    #[test]
    fn a_matching_pair_is_stripped_once() {
        assert_eq!(
            strip_wrapping_quotes("\"token\""),
            Some("token".to_string())
        );
        assert_eq!(strip_wrapping_quotes("'token'"), Some("token".to_string()));
        assert_eq!(
            strip_wrapping_quotes("  \"  token  \"  "),
            Some("token".to_string())
        );
        // Only one pair: a doubly-wrapped value keeps its inner quotes.
        assert_eq!(
            strip_wrapping_quotes("\"\"token\"\""),
            Some("\"token\"".to_string())
        );
    }

    #[test]
    fn mismatched_or_one_sided_quotes_are_left_alone() {
        assert_eq!(
            strip_wrapping_quotes("\"token"),
            Some("\"token".to_string())
        );
        assert_eq!(
            strip_wrapping_quotes("token\""),
            Some("token\"".to_string())
        );
        assert_eq!(
            strip_wrapping_quotes("\"token'"),
            Some("\"token'".to_string())
        );
    }

    #[test]
    fn empty_and_quote_only_values_are_none() {
        assert_eq!(strip_wrapping_quotes(""), None);
        assert_eq!(strip_wrapping_quotes("   "), None);
        assert_eq!(strip_wrapping_quotes("\"\""), None);
        assert_eq!(strip_wrapping_quotes("''"), None);
        assert_eq!(strip_wrapping_quotes("\"   \""), None);
    }

    #[test]
    fn multibyte_content_is_not_sliced_mid_character() {
        assert_eq!(
            strip_wrapping_quotes("\"caf\u{e9}\""),
            Some("caf\u{e9}".to_string())
        );
        // A multibyte character alone: len() >= 2 in BYTES but it is one char, so
        // the quote test must fail on content rather than on length.
        assert_eq!(strip_wrapping_quotes("\u{e9}"), Some("\u{e9}".to_string()));
        assert_eq!(
            strip_wrapping_quotes("\u{1f600}"),
            Some("\u{1f600}".to_string())
        );
    }

    #[test]
    fn boundary_index_is_unchanged() {
        assert_eq!(floor_char_boundary("abcdef", 3), 3);
        assert_eq!(floor_char_boundary("héllo", 1), 1);
    }

    #[test]
    fn index_inside_a_multibyte_scalar_rounds_down() {
        // "é" occupies bytes 1..3, so byte 2 is interior.
        let text = "aéb";
        assert_eq!(floor_char_boundary(text, 2), 1);
        assert!(text.is_char_boundary(floor_char_boundary(text, 2)));
    }

    #[test]
    fn index_past_the_end_clamps_to_the_length() {
        assert_eq!(floor_char_boundary("abc", 99), 3);
        // A 4-byte scalar: every interior index must round back to 0.
        let emoji = "\u{1f600}";
        for index in 1..emoji.len() {
            assert_eq!(floor_char_boundary(emoji, index), 0);
        }
    }

    #[test]
    fn every_index_of_a_mixed_string_yields_a_sliceable_offset() {
        let text = "aé中\u{1f600}z";
        for index in 0..=text.len() + 2 {
            let end = floor_char_boundary(text, index);
            // The point of the helper: the result is always safe to slice at.
            let _ = &text[..end];
        }
    }
}
