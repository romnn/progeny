//! Parameter serialization, one function per location.
//!
//! Shipped into generated crates and compiled here, so the table is unit-tested in the progeny
//! workspace rather than only in whatever a consumer happens to generate.
//!
//! Everything takes a `serde_json::Value` rather than being generic over the parameter type. That
//! is a compile-cost decision and a correctness one at once: one non-generic body per style row
//! instead of one per (row × parameter type), and one place where each row's rule lives, so the
//! rule cannot drift between two instantiations of it.

#![allow(
    dead_code,
    reason = "this file is shipped into generated crates; inside progeny it exists to be compiled and tested"
)]

use std::fmt::Write as _;

use serde_json::Value;

/// A serialization style OpenAPI names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Form,
    Simple,
    Label,
    Matrix,
    SpaceDelimited,
    PipeDelimited,
    DeepObject,
}

/// The query pairs one parameter contributes.
///
/// Returned as pairs rather than as an assembled string so the HTTP client does the percent
/// encoding: a query encoder that is right about `+` and `&` is not worth writing twice.
#[must_use]
pub fn query_pairs(
    name: &str,
    value: &Value,
    style: Style,
    explode: bool,
) -> Vec<(String, String)> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => match (style, explode) {
            // One `name=` per element: the default, and what every server understands.
            (Style::Form, true) => items
                .iter()
                .filter(|item| !item.is_null())
                .map(|item| (name.to_owned(), scalar(item)))
                .collect(),
            (Style::SpaceDelimited, false) => joined(name, items, " "),
            (Style::PipeDelimited, false) => joined(name, items, "|"),
            // `spaceDelimited` and `pipeDelimited` with `explode: true` are indistinguishable from
            // exploded `form`, which is what the specification says they become.
            (_, true) => items
                .iter()
                .filter(|item| !item.is_null())
                .map(|item| (name.to_owned(), scalar(item)))
                .collect(),
            (_, false) => joined(name, items, ","),
        },
        Value::Object(members) => match (style, explode) {
            // `a[x]=1`: the one style that keeps the member names addressable.
            (Style::DeepObject, _) => members
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (format!("{name}[{key}]"), scalar(value)))
                .collect(),
            // Exploded form drops the parameter name entirely: each member becomes its own pair.
            (_, true) => members
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), scalar(value)))
                .collect(),
            (_, false) => {
                let flat: Vec<String> = members
                    .iter()
                    .flat_map(|(key, value)| [key.clone(), scalar(value)])
                    .collect();
                if flat.is_empty() {
                    Vec::new()
                } else {
                    vec![(name.to_owned(), flat.join(","))]
                }
            }
        },
        scalar_value => vec![(name.to_owned(), scalar(scalar_value))],
    }
}

/// What one parameter puts in a path segment, already percent-encoded.
#[must_use]
pub fn path_segment(value: &Value, style: Style, explode: bool) -> String {
    let (prefix, separator, pair) = match style {
        Style::Label => (".", if explode { "." } else { "," }, explode),
        Style::Matrix => (";", if explode { ";" } else { "," }, true),
        // `simple`, and anything else that reaches a path: bare, comma separated.
        _ => ("", ",", explode),
    };
    let body = match value {
        Value::Null => String::new(),
        Value::Array(items) => items
            .iter()
            .map(|item| encode(&scalar(item)))
            .collect::<Vec<_>>()
            .join(separator),
        Value::Object(members) => members
            .iter()
            .map(|(key, value)| {
                if pair {
                    format!("{}={}", encode(key), encode(&scalar(value)))
                } else {
                    format!("{},{}", encode(key), encode(&scalar(value)))
                }
            })
            .collect::<Vec<_>>()
            .join(separator),
        other => encode(&scalar(other)),
    };
    format!("{prefix}{body}")
}

/// A matrix-style segment names the parameter; every other style does not.
#[must_use]
pub fn matrix_segment(name: &str, value: &Value, explode: bool) -> String {
    match value {
        Value::Array(items) if explode => {
            let mut out = String::new();
            for item in items {
                let _ = write!(out, ";{}={}", encode(name), encode(&scalar(item)));
            }
            out
        }
        Value::Object(members) if explode => {
            let mut out = String::new();
            for (key, value) in members {
                let _ = write!(out, ";{}={}", encode(key), encode(&scalar(value)));
            }
            out
        }
        Value::Null => format!(";{}", encode(name)),
        other => {
            let body = path_segment(other, Style::Simple, false);
            format!(";{}={body}", encode(name))
        }
    }
}

/// What one parameter puts in a header value.
///
/// Headers carry no brackets and no repetition rules, so this is `simple` and nothing else — which
/// is exactly what the style classifier admits for the location.
#[must_use]
pub fn header_value(value: &Value, explode: bool) -> String {
    match value {
        Value::Null => String::new(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(","),
        Value::Object(members) => members
            .iter()
            .map(|(key, value)| {
                if explode {
                    format!("{key}={}", scalar(value))
                } else {
                    format!("{key},{}", scalar(value))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        other => scalar(other),
    }
}

/// One `name=value` pair for the `Cookie` header.
#[must_use]
pub fn cookie_pair(name: &str, value: &Value) -> String {
    format!("{name}={}", header_value(value, false))
}

/// A scalar as the text it has on the wire.
///
/// A string contributes its own characters, not its JSON quoting: `?tag="cat"` is a different
/// request from `?tag=cat`, and only the second is what the document asked for.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn joined(name: &str, items: &[Value], separator: &str) -> Vec<(String, String)> {
    if items.is_empty() {
        return Vec::new();
    }
    let joined = items.iter().map(scalar).collect::<Vec<_>>().join(separator);
    vec![(name.to_owned(), joined)]
}

/// Percent-encode everything a path segment may not contain literally.
///
/// The unreserved set of RFC 3986 plus the sub-delimiters a segment is allowed to carry, which is
/// deliberately conservative: encoding a character that did not need it is a request that still
/// works, and failing to encode one that did is a request that goes somewhere else.
fn encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
            continue;
        }
        out.push('%');
        // `.get` rather than indexing: the shifts already prove both indices are below 16, and a
        // shipped file has no business being the one place a generated crate can panic.
        for nibble in [byte >> 4, byte & 0x0f] {
            if let Some(digit) = HEX.get(usize::from(nibble)) {
                out.push(char::from(*digit));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Style, cookie_pair, encode, header_value, matrix_segment, path_segment, query_pairs,
    };
    use serde_json::json;

    #[test]
    fn a_scalar_query_parameter_is_one_pair_with_no_json_quoting() {
        assert_eq!(
            query_pairs("tag", &json!("cat"), Style::Form, true),
            [("tag".to_owned(), "cat".to_owned())]
        );
        assert_eq!(
            query_pairs("limit", &json!(20), Style::Form, true),
            [("limit".to_owned(), "20".to_owned())]
        );
        // An absent value contributes nothing at all, rather than an empty pair.
        assert!(query_pairs("tag", &json!(null), Style::Form, true).is_empty());
    }

    #[test]
    fn an_array_explodes_or_joins_by_its_style() {
        let items = json!(["a", "b"]);
        assert_eq!(
            query_pairs("t", &items, Style::Form, true),
            [
                ("t".to_owned(), "a".to_owned()),
                ("t".to_owned(), "b".to_owned())
            ]
        );
        assert_eq!(
            query_pairs("t", &items, Style::Form, false),
            [("t".to_owned(), "a,b".to_owned())]
        );
        assert_eq!(
            query_pairs("t", &items, Style::SpaceDelimited, false),
            [("t".to_owned(), "a b".to_owned())]
        );
        assert_eq!(
            query_pairs("t", &items, Style::PipeDelimited, false),
            [("t".to_owned(), "a|b".to_owned())]
        );
    }

    #[test]
    fn an_object_keeps_its_member_names_addressable_under_deep_object() {
        let value = json!({"lat": 1, "lon": 2});
        assert_eq!(
            query_pairs("at", &value, Style::DeepObject, true),
            [
                ("at[lat]".to_owned(), "1".to_owned()),
                ("at[lon]".to_owned(), "2".to_owned())
            ]
        );
        // Exploded form drops the parameter name; unexploded form flattens into one pair.
        assert_eq!(
            query_pairs("at", &value, Style::Form, true),
            [
                ("lat".to_owned(), "1".to_owned()),
                ("lon".to_owned(), "2".to_owned())
            ]
        );
        assert_eq!(
            query_pairs("at", &value, Style::Form, false),
            [("at".to_owned(), "lat,1,lon,2".to_owned())]
        );
    }

    #[test]
    fn a_path_segment_is_encoded_and_prefixed_by_its_style() {
        assert_eq!(path_segment(&json!("a b"), Style::Simple, false), "a%20b");
        assert_eq!(path_segment(&json!("x"), Style::Label, false), ".x");
        assert_eq!(path_segment(&json!(["a", "b"]), Style::Label, true), ".a.b");
        assert_eq!(
            path_segment(&json!(["a", "b"]), Style::Simple, false),
            "a,b"
        );
        // A slash inside a value must not become a path separator.
        assert_eq!(path_segment(&json!("a/b"), Style::Simple, false), "a%2Fb");
    }

    #[test]
    fn a_matrix_segment_names_its_parameter() {
        assert_eq!(matrix_segment("id", &json!("x"), false), ";id=x");
        assert_eq!(matrix_segment("id", &json!(["a", "b"]), true), ";id=a;id=b");
        assert_eq!(matrix_segment("id", &json!(["a", "b"]), false), ";id=a,b");
    }

    #[test]
    fn a_header_is_comma_separated_and_never_percent_encoded() {
        assert_eq!(header_value(&json!("a b"), false), "a b");
        assert_eq!(header_value(&json!(["a", "b"]), false), "a,b");
        assert_eq!(header_value(&json!({"k": "v"}), true), "k=v");
        assert_eq!(header_value(&json!({"k": "v"}), false), "k,v");
        assert_eq!(cookie_pair("session", &json!("abc")), "session=abc");
    }

    #[test]
    fn encoding_covers_everything_outside_the_unreserved_set() {
        assert_eq!(encode("azAZ09-._~"), "azAZ09-._~");
        assert_eq!(encode("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
        // Multi-byte input is encoded byte by byte, which is what a URL carries.
        assert_eq!(encode("é"), "%C3%A9");
    }
}
