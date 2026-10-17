//! Whether a number literal arrives in the model as the document wrote it.
//!
//! With the `exact-numbers` feature, `serde_json` keeps every literal's text, so every number is
//! exact.
//! Without it a number is an `i64`, a `u64` or an `f64`, which cannot hold an integer beyond 64
//! bits or every decimal, and a number the model rounds has to be reported rather than rounded
//! silently.
//! The test is on the [`Number`] `serde_json` actually produced, so it holds whichever of its
//! parsers ran and whether or not another crate in the build turned arbitrary precision on.

use serde_json::{Number, Value};

use crate::diag::{Action, BreakageClass, Diagnostic, JsonPointer};

/// How a JSON number literal arrives in the model.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Held {
    /// As the document wrote it, up to spelling: `1e2` and `100.0` are one value.
    Exact(Number),
    /// As the nearest value the model can hold, which is a different number or a different kind
    /// of number.
    Rounded(Number),
    /// Not at all, because it lies beyond what the model can hold.
    Unrepresentable,
}

/// How the JSON number literal `literal` arrives in the model.
pub(super) fn hold(literal: &str) -> Held {
    let Ok(number) = serde_json::from_str::<Number>(literal) else {
        return Held::Unrepresentable;
    };
    // A literal written as an integer has to stay one, or its schema changes type.
    // Asked of the rendering rather than of `is_f64`, which arbitrary precision answers for
    // `-0` though it keeps the text.
    let held = number.to_string();
    let spelled_integral = |text: &str| !text.contains(['.', 'e', 'E']);
    if spelled_integral(literal) && !spelled_integral(&held) {
        return Held::Rounded(number);
    }
    // A literal kept as written is exact, whatever its exponent.
    // Arbitrary precision writes an exponent as `e+`, and a JSON literal has `+` only there.
    let spelling = |text: &str| text.replace('E', "e").replace('+', "");
    if spelling(&held) == spelling(literal) {
        return Held::Exact(number);
    }
    // A literal either side cannot normalize, such as one with an exponent beyond `i64`, is
    // not shown to be exact.
    match (Decimal::of(literal), Decimal::of(&held)) {
        (Some(written), Some(stored)) if written == stored => Held::Exact(number),
        _ => Held::Rounded(number),
    }
}

/// The member name a canonical number literal gives a key, the way arbitrary precision renders it.
///
/// A key is a name, so it keeps the literal rather than the value, and it is spelled the same
/// with or without arbitrary precision: an unsigned exponent gains its `+`, and `-0` is `0`.
pub(super) fn key_spelling(canonical: &str) -> String {
    if canonical == "-0" {
        return "0".to_owned();
    }
    match canonical.split_once('e') {
        Some((mantissa, exponent))
            if exponent.starts_with(|first: char| first.is_ascii_digit()) =>
        {
            format!("{mantissa}e+{exponent}")
        }
        _ => canonical.to_owned(),
    }
}

/// Whether `serde_json` keeps number literals as written in this build.
///
/// True with `exact-numbers`, and also when another crate in the build turns on
/// `arbitrary_precision`, which cargo unifies into progeny's `serde_json` too.
#[cfg(test)]
pub(super) fn arbitrary_precision() -> bool {
    serde_json::from_str::<Number>("18446744073709551616")
        .is_ok_and(|number| number.to_string() == "18446744073709551616")
}

/// The finding for a number the model holds only as `held`, the nearest value it can.
///
/// Folded on a fixed key, because the sentence names each literal and a document with a habit of
/// long decimals is one finding, not one per number.
pub(super) fn rounded(location: JsonPointer, literal: &str, held: &Value) -> Diagnostic {
    Diagnostic::new(
        BreakageClass::InexactNumber,
        Action::Degrade,
        location,
        format!(
            "`{literal}` is held as `{held}`, the nearest number progeny holds without the \
             `exact-numbers` feature"
        ),
    )
    .folded_as("inexact-number")
}

/// Report every number in a JSON document the model holds only as a nearest value.
///
/// `input` has parsed as JSON already, so this reads only as much of the grammar as it takes to
/// know where each number literal sits.
/// Each literal is parsed again on its own, by the parser that produced the loaded value, so the
/// finding is about the number the model holds.
#[cfg(not(feature = "exact-numbers"))]
pub(super) fn report_rounded_json(input: &[u8], ctx: &mut crate::diag::Ctx) {
    /// Where the scan is: inside an object, under its current key, or an array, at an index.
    enum Frame {
        Object { key: Option<String> },
        Array { index: usize },
    }
    let location = |frames: &[Frame]| {
        let mut pointer = JsonPointer::root();
        for frame in frames {
            match frame {
                Frame::Object { key: Some(key) } => pointer.push(key.clone()),
                Frame::Object { key: None } => {}
                Frame::Array { index } => pointer.push(index.to_string()),
            }
        }
        pointer
    };
    let mut frames: Vec<Frame> = Vec::new();
    // Whether the next string in an object is a key rather than a value.
    let mut expecting_key = false;
    let mut at = 0;
    while let Some(&byte) = input.get(at) {
        match byte {
            b'{' => {
                frames.push(Frame::Object { key: None });
                expecting_key = true;
                at += 1;
            }
            b'[' => {
                frames.push(Frame::Array { index: 0 });
                expecting_key = false;
                at += 1;
            }
            b'}' | b']' => {
                frames.pop();
                expecting_key = false;
                at += 1;
            }
            b',' => {
                match frames.last_mut() {
                    Some(Frame::Array { index }) => *index += 1,
                    Some(Frame::Object { key }) => {
                        *key = None;
                        expecting_key = true;
                    }
                    None => {}
                }
                at += 1;
            }
            b':' => {
                expecting_key = false;
                at += 1;
            }
            b'"' => {
                // The closing quote is the first one no backslash escapes.
                let mut end = at + 1;
                while let Some(&inner) = input.get(end) {
                    match inner {
                        b'\\' => end += 2,
                        b'"' => break,
                        _ => end += 1,
                    }
                }
                let text = input.get(at..=end).unwrap_or_default();
                if expecting_key && let Some(Frame::Object { key }) = frames.last_mut() {
                    *key = serde_json::from_slice::<String>(text).ok();
                }
                at = end + 1;
            }
            b'-' | b'0'..=b'9' => {
                let end = input
                    .get(at..)
                    .unwrap_or_default()
                    .iter()
                    .position(|byte| {
                        !matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                    })
                    .map_or(input.len(), |length| at + length);
                let literal = std::str::from_utf8(input.get(at..end).unwrap_or_default());
                if let Ok(literal) = literal
                    && let Held::Rounded(number) = hold(literal)
                {
                    ctx.report(rounded(location(&frames), literal, &Value::Number(number)));
                }
                at = end;
            }
            // Whitespace and the letters of `true`, `false` and `null`
            _ => at += 1,
        }
    }
}

/// A decimal value as its sign, its significant digits and the power of ten after them, so two
/// spellings of one value compare equal.
///
/// An integer literal that comes back as a float is caught before this compares anything.
#[derive(Debug, PartialEq, Eq)]
struct Decimal {
    negative: bool,
    digits: String,
    exponent: i64,
}

impl Decimal {
    /// The value a JSON number literal spells, or `None` when it is not one.
    fn of(literal: &str) -> Option<Self> {
        let (negative, magnitude) = match literal.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, literal),
        };
        let (mantissa, exponent) = magnitude
            .split_once(['e', 'E'])
            .unwrap_or((magnitude, "0"));
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

#[cfg(test)]
mod tests {
    use super::{Decimal, Held, hold};

    // Checked at run time, not by feature: another crate can turn arbitrary precision on.

    /// Spellings of one value are one decimal, zero's sign included.
    #[test]
    fn spellings_of_one_value_are_one_decimal() {
        for (one, other) in [
            ("100", "1e2"),
            ("1.0", "1"),
            ("0.30", "3e-1"),
            ("1.500", "15E-1"),
            ("0", "0.000"),
        ] {
            assert_eq!(Decimal::of(one), Decimal::of(other), "{one} and {other}");
        }
        assert_eq!(Decimal::of("-0.0"), Decimal::of("0"));
        assert_eq!(Decimal::of("0e9223372036854775808"), Decimal::of("0"));
        assert_ne!(Decimal::of("1.1"), Decimal::of("1.10000001"));
    }

    /// Without arbitrary precision, a literal that is a 64-bit integer or that `f64` renders
    /// back is exact, and anything else is rounded or beyond the model.
    #[test]
    fn a_literal_the_model_cannot_hold_is_rounded_or_beyond_it() {
        if super::arbitrary_precision() {
            return;
        }
        for literal in [
            "1",
            "-7",
            "18446744073709551615",
            "1.5",
            "1e2",
            "0.1",
            "-0.0",
        ] {
            assert!(matches!(hold(literal), Held::Exact(_)), "{literal}");
        }
        for literal in [
            "18446744073709551616",
            "0.30000000000000000000001",
            "12345678901234567890.5",
            "-0",
        ] {
            assert!(matches!(hold(literal), Held::Rounded(_)), "{literal}");
        }
        assert_eq!(hold("1e400"), Held::Unrepresentable);
        // An exponent beyond `i64` is not shown to be exact, rather than overflowing.
        assert!(matches!(hold("1.1e-9223372036854775808"), Held::Rounded(_)));
    }

    /// A key is spelled the way arbitrary precision renders its literal, in either build.
    #[test]
    fn a_key_is_spelled_as_arbitrary_precision_renders_it() {
        for (canonical, spelled) in [
            ("1e2", "1e+2"),
            ("1e-2", "1e-2"),
            ("1e02", "1e+02"),
            ("1.50e3", "1.50e+3"),
            ("-0", "0"),
            ("1.50", "1.50"),
        ] {
            assert_eq!(super::key_spelling(canonical), spelled, "{canonical}");
        }
    }

    /// With arbitrary precision every literal is held as written.
    #[test]
    fn every_literal_is_exact_with_arbitrary_precision() {
        if !super::arbitrary_precision() {
            return;
        }
        for literal in [
            "18446744073709551616",
            "0.30000000000000000000001",
            "-0",
            "1e400",
            "1e9223372036854775808",
            "1E+9223372036854775808",
        ] {
            let held = hold(literal);
            assert!(matches!(held, Held::Exact(_)), "{literal}: {held:?}");
        }
    }
}
