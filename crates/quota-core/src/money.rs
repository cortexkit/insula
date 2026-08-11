//! Parsing money and credit amounts off a provider's wire.
//!
//! Every provider examined sends amounts as decimal strings or as integer minor
//! units, and none sends a float -- which is the right instinct, because a
//! balance is compared against zero on every routing decision that reads it and
//! binary floating point cannot hold ordinary decimal amounts exactly.
//!
//! Shared rather than written per provider: two providers parsing money with
//! independently drifting rules is how one of them ends up lenient, and the
//! lenient direction here silently misstates somebody's balance.

use crate::model::Amount;

/// Parse a decimal string into integer minor units.
///
/// The provider's own precision is preserved rather than normalised: `"110.00"`
/// becomes 11000 at exponent 2, `"0.5"` becomes 5 at exponent 1, and `"10"`
/// becomes 10 at exponent 0. Rounding to a fixed two places would invent
/// precision for a provider that reports less and silently truncate one that
/// reports more.
///
/// Anything not exactly a decimal number is refused rather than salvaged, and
/// that matters more than it looks. A grouped figure like `"1,000.00"` parses to
/// `1` under a lenient reader — a thousandfold understatement that reads as a
/// perfectly ordinary balance, with no error anywhere. Refusing leaves the pool
/// unpublished, which a consumer treats as unknown; guessing produces a number
/// it would spend against.
pub fn parse_amount(raw: &str, unit: &str) -> Option<Amount> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }

    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text.strip_prefix('+').unwrap_or(text)),
    };

    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (digits, ""),
    };

    // Both halves must be digits only. This is what rejects grouped thousands,
    // currency symbols, exponent notation, and a second decimal point.
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    // A cap on stated precision, not a limit of the arithmetic. Nine decimal
    // places is far past what any money or credit figure carries, so a longer
    // fraction means the field is not the decimal number it appears to be, and
    // publishing it would give a consumer a figure whose scale nobody intended.
    // The digits themselves are concatenated and parsed, so an over-long value
    // would fail the `i64` parse below anyway -- this refuses it by its shape
    // rather than by whether it happens to fit.
    let exponent = u8::try_from(fraction.len()).ok()?;
    if exponent > 9 {
        return None;
    }

    let combined = format!("{whole}{fraction}");
    let magnitude: i64 = combined.parse().ok()?;

    Some(Amount {
        minor: sign * magnitude,
        exponent,
        unit: unit.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amounts keep the precision the provider stated, in integer minor units.
    #[test]
    fn amounts_parse_to_minor_units_at_the_stated_precision() {
        assert_eq!(
            parse_amount("110.00", "USD"),
            Some(Amount {
                minor: 11_000,
                exponent: 2,
                unit: "USD".to_string()
            })
        );
        // Fewer decimals is not padded: reporting exponent 2 here would claim a
        // precision the provider did not state.
        assert_eq!(
            parse_amount("0.5", "USD"),
            Some(Amount {
                minor: 5,
                exponent: 1,
                unit: "USD".to_string()
            })
        );
        assert_eq!(
            parse_amount("10", "credits"),
            Some(Amount {
                minor: 10,
                exponent: 0,
                unit: "credits".to_string()
            })
        );
        // A negative balance is a real state (an account in arrears) and must
        // survive as one rather than being clamped to zero, which would read as
        // an account that simply has nothing left.
        assert_eq!(
            parse_amount("-1.50", "USD"),
            Some(Amount {
                minor: -150,
                exponent: 2,
                unit: "USD".to_string()
            })
        );
    }

    /// A number this cannot read is refused, never salvaged.
    ///
    /// The grouped case is the one that matters and the reason the parse is
    /// strict rather than tolerant: `"1,000.00"` under a lenient reader becomes
    /// 1, a thousandfold understatement that looks like an ordinary balance and
    /// raises nothing. Refusing leaves the pool unpublished, which a consumer
    /// treats as unknown; salvaging produces a figure it would spend against.
    #[test]
    fn an_unreadable_amount_is_refused_rather_than_guessed() {
        for bad in [
            "1,000.00", // grouped thousands
            "$10.00",   // currency symbol
            "1e3",      // exponent notation
            "1.2.3",    // two decimal points
            "",         // empty
            "   ",      // whitespace only
            "abc",      // not a number
            ".",        // a point and no digits
        ] {
            assert_eq!(
                parse_amount(bad, "USD"),
                None,
                "{bad:?} must not parse to a spendable figure"
            );
        }
    }
}
