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

/// Where the round trip lost or changed something, if it did.
///
/// `expected` is what the document says a faithful round trip writes and `actual` what the type
/// wrote, both JSON text.
/// Numbers are compared by value, so `1` and `1.0` are one number, and `9007199254740993` and
/// `9007199254740992.0` are two.
/// Strings are compared by their text, whatever their escapes.
pub(crate) fn difference(expected: &str, actual: &str) -> Option<String> {
    match (
        serde_json::from_str::<&RawValue>(expected),
        serde_json::from_str::<&RawValue>(actual),
    ) {
        (Ok(expected), Ok(actual)) => walk(expected, actual, ""),
        (Err(error), _) | (_, Err(error)) => Some(format!("the payload is not JSON: {error}")),
    }
}

fn walk(expected: &RawValue, actual: &RawValue, at: &str) -> Option<String> {
    let (left, right) = (expected.get(), actual.get());
    match (left.as_bytes().first(), right.as_bytes().first()) {
        (Some(b'{'), Some(b'{')) => objects(left, right, at),
        (Some(b'['), Some(b'[')) => arrays(left, right, at),
        (Some(b'"'), Some(b'"')) => strings(left, right, at),
        // The same token, or two spellings of one number.
        _ if left == right => None,
        _ => match (number(left), number(right)) {
            (Some(expected), Some(actual)) if expected == actual => None,
            _ => Some(changed(at, left, right)),
        },
    }
}

fn objects(expected: &str, actual: &str, at: &str) -> Option<String> {
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
                if let Some(reason) = walk(value, other, &inner) {
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

fn arrays(expected: &str, actual: &str, at: &str) -> Option<String> {
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
        .find_map(|(index, (value, other))| walk(value, other, &child(at, &index.to_string())))
}

/// Two strings are one when they unescape to the same text.
fn strings(expected: &str, actual: &str, at: &str) -> Option<String> {
    match (
        serde_json::from_str::<String>(expected),
        serde_json::from_str::<String>(actual),
    ) {
        (Ok(left), Ok(right)) if left == right => None,
        _ => Some(changed(at, expected, actual)),
    }
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
