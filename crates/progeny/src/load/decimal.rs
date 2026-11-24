//! The value a JSON number literal spells, whatever its spelling.
//!
//! The loader compiles this file to tell an exact literal from a rounded one.
//! The payload gate compiles the same file into its own tests and ships it into every payload
//! test, where it tells a number the round trip kept from one it changed.
//! So it names nothing outside the standard library, and its tests live with the loader.

/// A decimal value as its sign, its significant digits and the power of ten after them, so two
/// spellings of one value compare equal.
///
/// An integer literal that comes back as a float is caught before this compares anything.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Decimal {
    negative: bool,
    digits: String,
    exponent: i64,
}

impl Decimal {
    /// The value a JSON number literal spells, or `None` when it is not one.
    pub(super) fn of(literal: &str) -> Option<Self> {
        let (negative, magnitude) = match literal.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, literal),
        };
        let (mantissa, exponent) = magnitude.split_once(['e', 'E']).unwrap_or((magnitude, "0"));
        let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let all: String = integer.chars().chain(fraction.chars()).collect();
        // Zeros before the first significant digit say nothing; zeros after the last one move
        // the exponent.
        let leading = all.trim_start_matches('0');
        let digits = leading.trim_end_matches('0');
        // Zero is one value whatever its sign and exponent: `serde_json` reads `-0` as `0` even
        // with arbitrary precision, and an exponent too long for `i64` still scales nothing.
        if digits.is_empty() {
            return Some(Self {
                negative: false,
                digits: String::new(),
                exponent: 0,
            });
        }
        let fraction_length = i64::try_from(fraction.len()).ok()?;
        let trailing = i64::try_from(leading.len() - digits.len()).ok()?;
        Some(Self {
            negative,
            digits: digits.to_owned(),
            exponent: exponent
                .parse::<i64>()
                .ok()?
                .checked_sub(fraction_length)?
                .checked_add(trailing)?,
        })
    }
}
