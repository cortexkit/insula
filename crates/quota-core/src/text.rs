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

#[cfg(test)]
mod tests {
    use super::*;

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
