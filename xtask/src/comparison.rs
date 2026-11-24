//! The payload gate's comparison of what a document expects back with what a generated type wrote.
//!
//! The payload gate ships this file verbatim into every payload test, beside the loader's decimal
//! normalization in `decimal.rs`, and xtask compiles both for its own tests.
//! Each place names that file `decimal` beside this one, so `super::decimal` reaches it in both.
//!
//! Both sides stay JSON text until a leaf is compared.
//! A parsed number is an `f64` or a 64-bit integer, which holds neither every integer past 2^53
//! nor every decimal, so comparing parsed numbers would accept a type that rounds one.

use std::collections::BTreeMap;

use serde_json::value::RawValue;

use super::decimal::Decimal;

/// A string the expectation reached through a format, which the generated type may respell.
pub(crate) struct Formatted<'a> {
    /// Where the string sits, as a JSON Pointer into the payload.
    pub(crate) at: &'a str,
    /// Whether two spellings of it are one value of the type the generated field holds it in.
    pub(crate) equivalent: fn(&str, &str) -> bool,
}

/// Where the round trip lost or changed something, if it did.
///
/// `expected` is what the document says a faithful round trip writes and `actual` what the type
/// wrote, both JSON text.
/// Numbers are compared by value, so `1` and `1.0` are one number, and `9007199254740993` and
/// `9007199254740992.0` are two.
/// Strings are compared by their text, whatever their escapes, except where `formatted` says the
/// type reads both spellings as one value.
pub(crate) fn difference(expected: &str, actual: &str, formatted: &[Formatted]) -> Option<String> {
    match (
        serde_json::from_str::<&RawValue>(expected),
        serde_json::from_str::<&RawValue>(actual),
    ) {
        (Ok(expected), Ok(actual)) => walk(expected, actual, "", formatted),
        (Err(error), _) | (_, Err(error)) => Some(format!("the payload is not JSON: {error}")),
    }
}

/// Whether two spellings of one string read as one value of `T`.
///
/// Both are JSON string literals, and both have to read: a spelling the type refuses is not the
/// value the other one is.
pub(crate) fn same<T>(expected: &str, actual: &str) -> bool
where
    T: serde::de::DeserializeOwned + PartialEq,
{
    match (
        serde_json::from_str::<T>(expected),
        serde_json::from_str::<T>(actual),
    ) {
        (Ok(expected), Ok(actual)) => expected == actual,
        _ => false,
    }
}

/// [`same`], for a date-time or a time.
///
/// The types a clock reading is held in keep no digit past the nanosecond, and some read a leap
/// second as the second before it.
/// A spelling with a non-zero digit past the nanosecond, or with a 60th second, can read as a
/// value it does not spell, so it is never one value with another spelling.
pub(crate) fn same_clock<T>(expected: &str, actual: &str) -> bool
where
    T: serde::de::DeserializeOwned + PartialEq,
{
    let held = serde_json::from_str::<String>(expected).is_ok_and(|text| !beyond_types(&text));
    held && same::<T>(expected, actual)
}

/// Whether an RFC 3339 date-time or time spells a leap second or a non-zero digit past the
/// nanosecond.
fn beyond_types(text: &str) -> bool {
    // The clock follows the date, where there is one.
    let clock = text.rsplit(['T', 't']).next().unwrap_or(text);
    let Some(second) = clock.split(':').nth(2) else {
        return false;
    };
    let fraction = second.get(2..).and_then(|rest| rest.strip_prefix('.'));
    let past_nanoseconds = fraction.is_some_and(|fraction| {
        fraction
            .chars()
            .take_while(char::is_ascii_digit)
            .skip(9)
            .any(|digit| digit != '0')
    });
    second.starts_with("60") || past_nanoseconds
}

fn walk(
    expected: &RawValue,
    actual: &RawValue,
    at: &str,
    formatted: &[Formatted],
) -> Option<String> {
    let (left, right) = (expected.get(), actual.get());
    match (left.as_bytes().first(), right.as_bytes().first()) {
        (Some(b'{'), Some(b'{')) => objects(left, right, at, formatted),
        (Some(b'['), Some(b'[')) => arrays(left, right, at, formatted),
        (Some(b'"'), Some(b'"')) => strings(left, right, at, formatted),
        // The same token, or two spellings of one number.
        _ if left == right => None,
        _ => match (number(left), number(right)) {
            (Some(expected), Some(actual)) if expected == actual => None,
            _ => Some(changed(at, left, right)),
        },
    }
}

fn objects(expected: &str, actual: &str, at: &str, formatted: &[Formatted]) -> Option<String> {
    let (expected, actual) = match (
        serde_json::from_str::<BTreeMap<String, &RawValue>>(expected),
        serde_json::from_str::<BTreeMap<String, &RawValue>>(actual),
    ) {
        (Ok(expected), Ok(actual)) => (expected, actual),
        (Err(error), _) | (_, Err(error)) => return Some(format!("`{at}` is not JSON: {error}")),
    };
    for (key, value) in &expected {
        let inner = child(at, key);
        match actual.get(key) {
            Some(other) => {
                if let Some(reason) = walk(value, other, &inner, formatted) {
                    return Some(reason);
                }
            }
            None => return Some(format!("the round trip dropped `{inner}`")),
        }
    }
    actual
        .keys()
        .find(|key| !expected.contains_key(*key))
        .map(|key| format!("the round trip invented `{}`", child(at, key)))
}

fn arrays(expected: &str, actual: &str, at: &str, formatted: &[Formatted]) -> Option<String> {
    let (expected, actual) = match (
        serde_json::from_str::<Vec<&RawValue>>(expected),
        serde_json::from_str::<Vec<&RawValue>>(actual),
    ) {
        (Ok(expected), Ok(actual)) => (expected, actual),
        (Err(error), _) | (_, Err(error)) => return Some(format!("`{at}` is not JSON: {error}")),
    };
    if expected.len() != actual.len() {
        return Some(format!(
            "`{at}` had {} elements and came back with {}",
            expected.len(),
            actual.len()
        ));
    }
    expected
        .iter()
        .zip(&actual)
        .enumerate()
        .find_map(|(index, (value, other))| {
            walk(value, other, &child(at, &index.to_string()), formatted)
        })
}

/// Two strings are one when they unescape to the same text, or when the type a formatted string
/// is held in reads both spellings as one value.
fn strings(expected: &str, actual: &str, at: &str, formatted: &[Formatted]) -> Option<String> {
    let same_text = matches!(
        (
            serde_json::from_str::<String>(expected),
            serde_json::from_str::<String>(actual),
        ),
        (Ok(left), Ok(right)) if left == right
    );
    if same_text {
        return None;
    }
    let equivalent = formatted
        .iter()
        .find(|leaf| leaf.at == at)
        .is_some_and(|leaf| (leaf.equivalent)(expected, actual));
    (!equivalent).then(|| changed(at, expected, actual))
}

/// The value a number token spells, or `None` when the token is not a number.
fn number(token: &str) -> Option<Decimal> {
    token
        .starts_with(|first: char| first == '-' || first.is_ascii_digit())
        .then(|| Decimal::of(token))
        .flatten()
}

/// The JSON Pointer to `token` inside `at`.
fn child(at: &str, token: &str) -> String {
    format!("{at}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn changed(at: &str, expected: &str, actual: &str) -> String {
    format!("`{at}` was {expected} and came back {actual}")
}
