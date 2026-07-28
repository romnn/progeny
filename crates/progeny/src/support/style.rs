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
            // exploded `form`, which is what the specification says they become. Spelled out
            // rather than wildcarded: a style added to the enum and not to this table would
            // otherwise be silently encoded as `form`, which is a request line the document did
            // not ask for.
            (
                Style::Simple
                | Style::Label
                | Style::Matrix
                | Style::SpaceDelimited
                | Style::PipeDelimited
                | Style::DeepObject,
                true,
            ) => items
                .iter()
                .filter(|item| !item.is_null())
                .map(|item| (name.to_owned(), scalar(item)))
                .collect(),
            (
                Style::Form | Style::Simple | Style::Label | Style::Matrix | Style::DeepObject,
                false,
            ) => joined(name, items, ","),
        },
        Value::Object(members) => match (style, explode) {
            // `a[x]=1`: the one style that keeps the member names addressable.
            (Style::DeepObject, _) => members
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (format!("{name}[{key}]"), scalar(value)))
                .collect(),
            // Exploded form drops the parameter name entirely: each member becomes its own pair.
            (
                Style::Form
                | Style::Simple
                | Style::Label
                | Style::Matrix
                | Style::SpaceDelimited
                | Style::PipeDelimited,
                true,
            ) => members
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), scalar(value)))
                .collect(),
            (
                Style::Form
                | Style::Simple
                | Style::Label
                | Style::Matrix
                | Style::SpaceDelimited
                | Style::PipeDelimited,
                false,
            ) => {
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

/// What the document said about one member of a form body, when it said anything.
///
/// `encoding` on an `application/x-www-form-urlencoded` body is declared exactly once in the whole
/// corpus, and it sets `style` and `explode` — which is why those are the two fields here and there
/// is no third.
#[derive(Debug, Clone, Copy)]
pub struct FormSpec {
    pub name: &'static str,
    pub style: Style,
    pub explode: bool,
    /// Whether the member's schema declares an array, which the wire alone cannot say.
    ///
    /// `None` when the document declared the row without declaring the member — an `encoding`
    /// entry can name a member the schema only captures through `additionalProperties`, and a row
    /// that guessed `false` there made the reader drop every occurrence after the first. `None`
    /// hands the question to the repetition heuristic, exactly as if the row were absent.
    pub array: Option<bool>,
}

/// An `application/x-www-form-urlencoded` body.
///
/// The same encoder the query string uses, because they are the same encoding: a form body *is* a
/// query string in the body position. Writing it twice would be two chances to disagree about
/// arrays, and the style table already has the row.
///
/// Space becomes `%20` rather than `+`. Both are accepted everywhere — `+` is the older form and
/// `%20` is what percent-encoding means — and reusing one encoder is worth more than matching a
/// convention that has no reader which rejects the alternative.
#[must_use]
pub fn form_body(value: &Value, specs: &[FormSpec]) -> String {
    let Some(members) = value.as_object() else {
        // A form body that is not an object has no member names, and inventing one would be
        // inventing the wire format. An empty body says what actually happened.
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (name, member) in members {
        // The default for a form body is exploded `form`, which is the same default a query
        // parameter has and for the same reason.
        let (style, explode) = specs
            .iter()
            .find(|spec| spec.name == name)
            .map_or((Style::Form, true), |spec| (spec.style, spec.explode));
        // An object-valued member goes as one `name=<json>` pair rather than through the query
        // rule. Exploding an object is *spec'd* behaviour for a query parameter — the erasure of
        // the parameter's name is what `explode` means there — but as a default for a body member
        // it writes the member's contents under bare keys no reader can ever reassemble, which the
        // wire probe demonstrated on `posthog`'s `Table`: three nested members exploded into each
        // other. JSON text is the one spelling whose reader is exact, and it is the same rule a
        // compound query element already follows. `deepObject` keeps its addressable form.
        if matches!(member, Value::Object(_)) && style != Style::DeepObject {
            pairs.push((name.clone(), scalar(member)));
            continue;
        }
        pairs.extend(query_pairs(name, member, style, explode));
    }
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Read one parameter back out of a query string.
///
/// The inverse of [`query_pairs`], and it lives beside it for the reason the encoder lives in one
/// place: a client that writes `a=1&a=2` and a server that reads it are two halves of one row, and
/// two files would be two chances for the row to be read differently at each end. The round-trip is
/// asserted per row in this module's tests.
///
/// `None` means the query string did not carry the parameter at all, which is a different answer
/// from "carried it empty" and the caller decides what each costs.
#[must_use]
pub fn from_query(
    query: &str,
    name: &str,
    style: Style,
    explode: bool,
    array: bool,
) -> Option<Value> {
    let pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect();

    if style == Style::DeepObject {
        // `a[x]=1`: the members are addressable, so the object is rebuilt from the keys.
        let prefix = format!("{name}[");
        let members: serde_json::Map<String, Value> = pairs
            .iter()
            .filter_map(|(key, value)| {
                let member = key.strip_prefix(&prefix)?.strip_suffix(']')?;
                Some((member.to_owned(), Value::String(value.clone())))
            })
            .collect();
        return (!members.is_empty()).then_some(Value::Object(members));
    }

    let mine: Vec<&String> = pairs
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value)
        .collect();
    let first = mine.first()?;
    // The schema decides *whether* this is an array and the row decides *how* one is spelled —
    // neither alone is enough, which the wire probe proved on `jellyfin`: `ids=a,b` under
    // `style: form, explode: false` is byte-identical to a scalar that contains a comma, so a
    // reader working from the row alone either splits scalars it must not or returns arrays as
    // one joined string, and this returned the string. The same shape-blindness made a
    // single-element exploded array — `ids=a`, one occurrence — indistinguishable from a scalar.
    // A URL carries no types; the schema supplies them, here as everywhere.
    Some(if array {
        match (style, explode) {
            (Style::SpaceDelimited, false) => split_one(first, ' '),
            (Style::PipeDelimited, false) => split_one(first, '|'),
            // `simple`, `label` and `matrix` do not reach a query string, but the row is data a
            // caller supplies; comma-joined is what each of them does to an array elsewhere.
            (
                Style::Form | Style::DeepObject | Style::Simple | Style::Label | Style::Matrix,
                false,
            ) => split_one(first, ','),
            // Exploded: one occurrence per element, however many arrived — a one-element array
            // reaches the handler as a one-element array, not as its element.
            (
                Style::Form
                | Style::Simple
                | Style::Label
                | Style::Matrix
                | Style::SpaceDelimited
                | Style::PipeDelimited
                | Style::DeepObject,
                true,
            ) => split_values(&mine),
        }
    } else {
        // A scalar is the first occurrence's text, commas and all.
        Value::String((*first).clone())
    })
}

/// Repeated occurrences as the array they spell. The separator is the repetition itself, which is
/// why this takes none: the values arrived already apart.
fn split_values(values: &[&String]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| Value::String((*value).clone()))
            .collect(),
    )
}

fn split_one(value: &str, separator: char) -> Value {
    Value::Array(
        value
            .split(separator)
            .map(|part| Value::String(part.to_owned()))
            .collect(),
    )
}

/// Undo the percent-encoding a query string carries.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes.get(index) {
            // `+` is the older spelling of a space in a form-encoded query. progeny writes `%20`,
            // but it reads both: what arrives was written by somebody else's client.
            Some(b'+') => out.push(b' '),
            Some(b'%') => {
                let hex = text
                    .get(index + 1..index + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                        continue;
                    }
                    // A stray `%` that is not an escape is a literal `%`, which is what every
                    // lenient reader does and the only alternative to refusing the request.
                    None => out.push(b'%'),
                }
            }
            Some(byte) => out.push(*byte),
            None => break,
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a whole form-urlencoded body back into an object.
///
/// The inverse of [`form_body`], member by member, through the same rows. Every member arrives as
/// text — a form body has no types either — and the caller asks serde for the shape afterwards.
#[must_use]
pub fn query_object(query: &str, specs: &[FormSpec]) -> Value {
    let mut members = serde_json::Map::new();
    for name in query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('=').map(|(key, _)| decode(key)))
    {
        if members.contains_key(&name) {
            continue;
        }
        let spec = specs.iter().find(|spec| spec.name == name);
        let (style, explode) = spec.map_or((Style::Form, true), |spec| (spec.style, spec.explode));
        // The spec table carries every declared member's shape, derived from the body's own
        // contract — the wire probe caught the cost of guessing on `posthog`, where a one-element
        // array member is byte-identical to a scalar. The repetition heuristic survives only where
        // no schema answered: a member outside the table, or a row whose member the schema only
        // captures — repetition is the one signal the wire itself offers there.
        let array = spec.and_then(|spec| spec.array).unwrap_or_else(|| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .filter(|(key, _)| decode(key) == name)
                .count()
                > 1
        });
        if let Some(value) = from_query(query, &name, style, explode, array) {
            members.insert(name, value);
        }
    }
    Value::Object(members)
}

/// What one parameter puts in a path segment, already percent-encoded.
#[must_use]
pub fn path_segment(value: &Value, style: Style, explode: bool) -> String {
    let (prefix, separator, pair) = match style {
        Style::Label => (".", if explode { "." } else { "," }, explode),
        Style::Matrix => (";", if explode { ";" } else { "," }, true),
        // `simple`, and anything else that reaches a path: bare, comma separated.
        Style::Form
        | Style::Simple
        | Style::SpaceDelimited
        | Style::PipeDelimited
        | Style::DeepObject => ("", ",", explode),
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
        FormSpec, Style, cookie_pair, encode, form_body, header_value, matrix_segment,
        path_segment, query_object, query_pairs,
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

    /// Encode a parameter the way a generated client does, then read it the way a generated server
    /// does, and assert the row survives the round trip.
    fn round_trip(
        value: &json_types::Value,
        style: Style,
        explode: bool,
    ) -> Option<json_types::Value> {
        let query = query_pairs("p", value, style, explode)
            .iter()
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        super::from_query(&query, "p", style, explode, value.is_array())
    }

    use serde_json as json_types;

    #[test]
    fn every_style_row_survives_the_client_writing_it_and_the_server_reading_it() {
        // The one assertion that makes the table a table rather than two lists. Values come back as
        // strings because a query string has no types — what a parameter *is* comes from the
        // schema, and `read` asks serde for it afterwards.
        assert_eq!(
            round_trip(&json!("plain value"), Style::Form, true),
            Some(json!("plain value"))
        );
        assert_eq!(
            round_trip(&json!(["a", "b"]), Style::Form, true),
            Some(json!(["a", "b"]))
        );
        assert_eq!(
            round_trip(&json!(["a", "b"]), Style::SpaceDelimited, false),
            Some(json!(["a", "b"]))
        );
        assert_eq!(
            round_trip(&json!(["a", "b"]), Style::PipeDelimited, false),
            Some(json!(["a", "b"]))
        );
        assert_eq!(
            round_trip(&json!({"x": "1", "y": "2"}), Style::DeepObject, true),
            Some(json!({"x": "1", "y": "2"}))
        );
    }

    #[test]
    fn a_parameter_the_query_never_carried_is_absent_rather_than_empty() {
        // The distinction the whole optional-parameter rule rests on: a server has to tell "not
        // sent" from "sent empty", and only `None` says the first.
        assert_eq!(super::from_query("", "p", Style::Form, true, false), None);
        assert_eq!(
            super::from_query("q=1", "p", Style::Form, true, false),
            None
        );
        assert_eq!(
            super::from_query("p=", "p", Style::Form, true, false),
            Some(json!(""))
        );
    }

    #[test]
    fn what_arrives_percent_encoded_comes_back_as_it_was_written() {
        assert_eq!(
            super::from_query("p=a%20b%26c", "p", Style::Form, true, false),
            Some(json!("a b&c"))
        );
        // `+` is the older spelling of a space. progeny writes `%20` and reads both, because what
        // arrives was written by somebody else's client.
        assert_eq!(
            super::from_query("p=a+b", "p", Style::Form, true, false),
            Some(json!("a b"))
        );
        assert_eq!(
            super::from_query("p=100%25", "p", Style::Form, true, false),
            Some(json!("100%"))
        );
        // A `%` that is not an escape is a `%`, which is the only alternative to refusing.
        assert_eq!(
            super::from_query("p=50%zz", "p", Style::Form, true, false),
            Some(json!("50%zz"))
        );
    }

    #[test]
    fn a_form_body_is_a_query_string_in_the_body_position() {
        assert_eq!(
            form_body(
                &json!({"grant_type": "client_credentials", "scope": "read"}),
                &[]
            ),
            "grant_type=client_credentials&scope=read"
        );
        // Every value is encoded, which is the difference from the query case: there the HTTP
        // client does it, and here there is no client between this and the socket.
        assert_eq!(
            form_body(&json!({"redirect uri": "https://example.test/cb?a=1"}), &[]),
            "redirect%20uri=https%3A%2F%2Fexample.test%2Fcb%3Fa%3D1"
        );
    }

    #[test]
    fn a_form_bodys_array_explodes_by_default() {
        assert_eq!(form_body(&json!({"tag": ["a", "b"]}), &[]), "tag=a&tag=b");
    }

    #[test]
    fn a_form_bodys_encoding_selects_a_style_row() {
        // The one corpus body that declares `encoding` on a form body sets `style` and `explode`,
        // and this is what that declaration has to do.
        let specs = [FormSpec {
            name: "tag",
            style: Style::SpaceDelimited,
            explode: false,
            array: Some(true),
        }];
        assert_eq!(form_body(&json!({"tag": ["a", "b"]}), &specs), "tag=a%20b");
    }

    /// A row that does not know the member's shape hands the question back to repetition.
    ///
    /// An `encoding` entry can name a member the schema only captures through
    /// `additionalProperties`, so the row carries `style` and `explode` and no shape. A guessed
    /// `array: false` here made the reader keep only the first occurrence — adding an `encoding`
    /// entry to a document silently dropped data its absence had preserved.
    #[test]
    fn a_spec_without_a_shape_lets_repetition_decide() {
        let specs = [FormSpec {
            name: "tag",
            style: Style::Form,
            explode: true,
            array: None,
        }];
        assert_eq!(
            query_object("tag=a&tag=b", &specs),
            json!({"tag": ["a", "b"]})
        );
        assert_eq!(query_object("tag=a", &specs), json!({"tag": "a"}));
    }

    /// An object member keeps its name on the wire, holding its JSON.
    ///
    /// The wire probe's `posthog` find: exploding an object member wrote its *contents* under bare
    /// keys — three nested members exploded into each other, and no reader can reassemble what has
    /// no name. One pair per member, JSON for the value, is the spelling whose reader is exact.
    #[test]
    fn an_object_member_of_a_form_body_survives_as_one_pair() {
        let body = form_body(&json!({"cfg": {"a": 1}, "name": "x"}), &[]);
        assert_eq!(body, "cfg=%7B%22a%22%3A1%7D&name=x");
        let read = super::query_object(&body, &[]);
        assert_eq!(read["cfg"], json!("{\"a\":1}"));
        assert_eq!(read["name"], json!("x"));
    }

    #[test]
    fn a_null_member_is_absent_from_a_form_body() {
        // Same rule as a query parameter, and for the same reason: a server cannot tell an empty
        // value from an unset one.
        assert_eq!(form_body(&json!({"a": null, "b": 1}), &[]), "b=1");
    }

    #[test]
    fn a_form_body_that_is_not_an_object_is_empty() {
        assert_eq!(form_body(&json!([1, 2]), &[]), "");
        assert_eq!(form_body(&json!("text"), &[]), "");
    }
}
