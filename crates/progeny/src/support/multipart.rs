//! `multipart/form-data` bodies.
//!
//! Shipped into generated crates and compiled here, so the format is unit-tested in the progeny
//! workspace rather than only in whatever a consumer happens to generate.
//!
//! The body is assembled from a `serde_json::Value` and a table of part specifications, rather than
//! from code unrolled per operation. Two reasons, and they are the same two as the style table: one
//! non-generic body instead of one per operation is compile time a consumer does not pay, and one
//! place where the format's rules live is one place they can be wrong.

#![allow(
    dead_code,
    reason = "this file is shipped into generated crates; inside progeny it exists to be compiled and tested"
)]

use std::fmt::Write as _;

use serde_json::Value;

/// How a member of the body becomes a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// Written as its text: what a scalar member is.
    Text,
    /// Written as its bytes, with a `filename`: what a member the document marked binary is.
    ///
    /// The member's Rust type is still `String`, because inside a JSON payload a `format: binary`
    /// property *is* a string and the type layer does not know which position it will be used in.
    /// The bytes that string holds are what goes on the wire.
    File,
    /// Written as JSON: what a structured member is.
    ///
    /// This is not progeny inventing a convention. Of the 13 corpus bodies that declare an
    /// `encoding` at all, every one that names a `contentType` for a structured member names
    /// `application/json`.
    Json,
}

/// What one member of the body should become, when the document said something about it.
#[derive(Debug, Clone, Copy)]
pub struct PartSpec {
    /// The member's name on the wire.
    pub name: &'static str,
    /// What one part of this member holds.
    pub kind: PartKind,
    /// Whether the member was declared an array, and therefore becomes one part per element.
    ///
    /// Separate from the kind because they answer different questions — what a part holds, and how
    /// many there are. A member typed as arbitrary JSON that happens to hold an array is one part,
    /// because nothing declared it repeated.
    pub repeated: bool,
    /// The per-part content type, when the body's `encoding` declared one.
    pub content_type: Option<&'static str>,
}

/// The `Content-Type` header value and the body bytes.
///
/// Returned together because the boundary is chosen from the content and therefore cannot be known
/// before the body is built: a caller that set the header first would be describing a different
/// body than the one it sends.
#[must_use]
pub fn body(value: &Value, specs: &[PartSpec]) -> Option<(String, Vec<u8>)> {
    let members = value.as_object()?;
    let mut parts: Vec<(String, Part)> = Vec::with_capacity(members.len());
    for (name, member) in members {
        // A member the document did not describe still goes on the wire. The specification table
        // says what the document was specific about; it is not the list of what exists, because an
        // `additionalProperties` member or a flattened one is real and absent from it.
        if member.is_null() {
            continue;
        }
        let spec = specs.iter().find(|spec| spec.name == name);
        let kind = spec.map_or_else(|| infer(member), |spec| spec.kind);
        let repeated = spec.is_some_and(|spec| spec.repeated);
        for part in split(
            member,
            kind,
            repeated,
            spec.and_then(|spec| spec.content_type),
        ) {
            parts.push((name.clone(), part));
        }
    }
    let boundary = boundary(&parts);
    Some((
        format!("multipart/form-data; boundary={boundary}"),
        write(&boundary, &parts),
    ))
}

/// The boundary a `Content-Type` header declares, if it declares one.
#[must_use]
pub fn boundary_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        (name.trim().eq_ignore_ascii_case("boundary"))
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Read a multipart body back into the object its parts came from.
///
/// The inverse of [`body`], in the same file for the reason the style table's decoder sits beside
/// its encoder: a client that writes a repeated part and a server that reads it are two halves of
/// one rule, and two files are two chances to disagree about it.
///
/// Every part arrives as text, because a multipart body carries no types — what a member *is* comes
/// from the schema, and the caller asks serde for it afterwards. A part the specification table
/// calls `Json` is parsed, because that part was written as JSON and reading it as a string would
/// hand serde a string where it wants an object.
///
/// # Errors
/// When the body is not well-formed multipart for the boundary given.
pub fn parse(body: &[u8], boundary: &str, specs: &[PartSpec]) -> Result<Value, String> {
    let delimiter = format!("--{boundary}");
    let mut members = serde_json::Map::new();
    for section in split_on(body, delimiter.as_bytes()).skip(1) {
        // The trailer after the closing delimiter is `--`, and anything after that is epilogue.
        if section.starts_with(b"--") {
            break;
        }
        let Some(headers_end) = find(section, b"\r\n\r\n") else {
            return Err("a multipart section has no header block".to_owned());
        };
        let headers = String::from_utf8_lossy(section.get(..headers_end).unwrap_or_default());
        let content = section
            .get(headers_end + 4..)
            .unwrap_or_default()
            .strip_suffix(b"\r\n")
            .unwrap_or_default();
        let Some(name) = disposition_name(&headers) else {
            return Err("a multipart section declares no name".to_owned());
        };
        let kind = specs
            .iter()
            .find(|spec| spec.name == name)
            .map_or(PartKind::Text, |spec| spec.kind);
        let repeated = specs
            .iter()
            .find(|spec| spec.name == name)
            .is_some_and(|spec| spec.repeated);
        let text = String::from_utf8_lossy(content).into_owned();
        let value = match kind {
            PartKind::Json => serde_json::from_str(&text).map_err(|source| source.to_string())?,
            PartKind::Text | PartKind::File => Value::String(text),
        };
        match members.get_mut(&name) {
            // A name seen twice is a repeated member, whatever the table said: what arrived is what
            // arrived, and dropping the first would lose a part the client meant to send.
            Some(Value::Array(existing)) => existing.push(value),
            Some(existing) => *existing = Value::Array(vec![existing.take(), value]),
            None if repeated => {
                members.insert(name, Value::Array(vec![value]));
            }
            None => {
                members.insert(name, value);
            }
        }
    }
    Ok(Value::Object(members))
}

/// The `name` of a section, from its `Content-Disposition`.
fn disposition_name(headers: &str) -> Option<String> {
    let line = headers.lines().find(|line| {
        line.split(':')
            .next()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("content-disposition"))
    })?;
    let start = line.find("name=\"")? + "name=\"".len();
    let rest = line.get(start..)?;
    // Undo the escaping the writer applies, so a name with a quote in it survives the round trip.
    let mut out = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(out),
            '\\' => out.push(characters.next()?),
            other => out.push(other),
        }
    }
    None
}

/// The body split on a delimiter, each piece with its leading `\r\n` removed.
fn split_on<'a>(body: &'a [u8], delimiter: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    let mut rest = Some(body);
    std::iter::from_fn(move || {
        let current = rest?;
        // The tail after the last delimiter is still a piece, which is why the `None` arm yields
        // rather than ending the iterator: a body whose final boundary is missing its trailing
        // `--` would otherwise lose its last part silently.
        let piece = if let Some(at) = find(current, delimiter) {
            rest = current.get(at + delimiter.len()..);
            current.get(..at).unwrap_or_default()
        } else {
            rest = None;
            current
        };
        Some(piece.strip_prefix(b"\r\n").unwrap_or(piece))
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// What a member with nothing said about it should become.
///
/// A structured member has no faithful text form, so it becomes JSON; everything else is its own
/// text. Guessing `File` is never right here — a member is binary because the document said so.
fn infer(member: &Value) -> PartKind {
    match member {
        Value::Array(_) | Value::Object(_) => PartKind::Json,
        _ => PartKind::Text,
    }
}

/// One part, before a boundary exists.
struct Part {
    filename: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// The parts one member contributes.
///
/// A repeated member is one part per element under one name, which is how a multi-file upload and a
/// repeated field are both written. The *kind* is the element's kind either way — 3.1's rule, "an
/// array: the default is defined based on the type of the item". 3.0 said `application/json` for
/// any array, which would put a JSON array where a server looks for several fields.
fn split(member: &Value, kind: PartKind, repeated: bool, content_type: Option<&str>) -> Vec<Part> {
    match member {
        Value::Array(items) if repeated => items
            .iter()
            .filter(|item| !item.is_null())
            .map(|item| one(item, kind, content_type))
            .collect(),
        single => vec![one(single, kind, content_type)],
    }
}

fn one(value: &Value, kind: PartKind, content_type: Option<&str>) -> Part {
    match kind {
        PartKind::File => file(value, content_type),
        PartKind::Text => text(value, content_type),
        PartKind::Json => Part {
            filename: None,
            content_type: Some(content_type.unwrap_or("application/json").to_owned()),
            body: value.to_string().into_bytes(),
        },
    }
}

fn text(value: &Value, content_type: Option<&str>) -> Part {
    Part {
        filename: None,
        content_type: content_type.map(ToOwned::to_owned),
        body: scalar(value).into_bytes(),
    }
}

fn file(value: &Value, content_type: Option<&str>) -> Part {
    Part {
        // A `filename` is what makes a server treat the part as an upload rather than a field, and
        // the document never says what it is. Absent would be faithful and useless; this is the
        // conventional stand-in, and a caller who needs a real name is describing a parameter the
        // document did not declare.
        filename: Some("file".to_owned()),
        content_type: Some(
            content_type
                .unwrap_or("application/octet-stream")
                .to_owned(),
        ),
        body: scalar(value).into_bytes(),
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The stem every boundary is built from, so a generated request is recognisable in a capture.
const STEM: &str = "progeny-boundary";

/// A boundary that appears in none of the parts.
///
/// Chosen by scanning rather than by drawing a random one. A random boundary is wrong with some
/// probability and the failure is a silently corrupted body — the one failure mode this project
/// refuses — while a scanned one is correct by construction and, as a bonus, makes a generated
/// request reproducible and therefore testable.
///
/// It terminates: each attempt is strictly longer than the last, and a finite body cannot contain a
/// string longer than itself.
fn boundary(parts: &[(String, Part)]) -> String {
    let mut candidate = STEM.to_owned();
    let mut attempt = 0u32;
    while parts
        .iter()
        .any(|(_, part)| contains(&part.body, candidate.as_bytes()))
    {
        attempt += 1;
        candidate = format!("{STEM}-{attempt}");
    }
    candidate
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn write(boundary: &str, parts: &[(String, Part)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, part) in parts {
        let mut headers = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"",
            quoted(name)
        );
        if let Some(filename) = &part.filename {
            let _ = write!(headers, "; filename=\"{}\"", quoted(filename));
        }
        headers.push_str("\r\n");
        if let Some(content_type) = &part.content_type {
            let _ = write!(headers, "Content-Type: {}\r\n", header_safe(content_type));
        }
        headers.push_str("\r\n");
        out.extend_from_slice(headers.as_bytes());
        out.extend_from_slice(&part.body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// A name as it may appear inside a quoted header parameter.
///
/// Backslash and quote are escaped and the line breaks are dropped. A property name comes from a
/// document rather than from a caller, but a header a document can inject into is a header a
/// document can add its own parts to, and refusing that costs one pass over a short string.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' | '"' => {
                out.push('\\');
                out.push(character);
            }
            '\r' | '\n' => {}
            other => out.push(other),
        }
    }
    out
}

/// A header value with the line breaks removed, for the same reason.
fn header_safe(text: &str) -> String {
    text.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PartKind, PartSpec, body};

    fn rendered(value: &serde_json::Value, specs: &[PartSpec]) -> String {
        let (_, bytes) = body(value, specs).expect("an object body");
        String::from_utf8(bytes).expect("utf-8 in these fixtures")
    }

    #[test]
    fn a_scalar_member_is_a_named_field() {
        let out = rendered(&json!({"name": "widget"}), &[]);
        assert_eq!(
            out,
            "--progeny-boundary\r\n\
             Content-Disposition: form-data; name=\"name\"\r\n\
             \r\n\
             widget\r\n\
             --progeny-boundary--\r\n"
        );
    }

    #[test]
    fn a_binary_member_carries_a_filename_and_a_content_type() {
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        let out = rendered(&json!({"file": "raw bytes"}), &specs);
        assert!(out.contains("name=\"file\"; filename=\"file\""), "{out}");
        assert!(
            out.contains("Content-Type: application/octet-stream"),
            "{out}"
        );
        assert!(out.contains("\r\n\r\nraw bytes\r\n"), "{out}");
    }

    #[test]
    fn a_declared_content_type_wins_over_the_default() {
        let specs = [PartSpec {
            name: "metadata",
            kind: PartKind::Json,
            repeated: false,
            content_type: Some("application/json"),
        }];
        let out = rendered(&json!({"metadata": {"a": 1}}), &specs);
        assert!(out.contains("Content-Type: application/json"), "{out}");
        assert!(out.contains("\r\n\r\n{\"a\":1}\r\n"), "{out}");
    }

    #[test]
    fn a_structured_member_nobody_described_is_json() {
        // The inference, and the reason it is not `Text`: `[1,2]` has no faithful text form, and
        // `to_string` on the `Value` would produce the JSON anyway without saying so in a header.
        let out = rendered(&json!({"tags": ["a", "b"]}), &[]);
        assert!(out.contains("Content-Type: application/json"), "{out}");
        assert!(out.contains("\r\n\r\n[\"a\",\"b\"]\r\n"), "{out}");
    }

    #[test]
    fn an_array_of_files_is_one_part_each() {
        // `anthropic` writes exactly this: `files` as an array of `format: binary` strings.
        let specs = [PartSpec {
            name: "files",
            kind: PartKind::File,
            repeated: true,
            content_type: None,
        }];
        let out = rendered(&json!({"files": ["first", "second"]}), &specs);
        assert_eq!(out.matches("name=\"files\"; filename=\"file\"").count(), 2);
        assert!(out.contains("\r\n\r\nfirst\r\n"), "{out}");
        assert!(out.contains("\r\n\r\nsecond\r\n"), "{out}");
    }

    #[test]
    fn an_array_of_scalars_is_one_part_each() {
        let out = rendered(
            &json!({"tag": ["a", "b"]}),
            &[PartSpec {
                name: "tag",
                kind: PartKind::Text,
                repeated: true,
                content_type: None,
            }],
        );
        assert_eq!(out.matches("name=\"tag\"").count(), 2);
        assert!(!out.contains("Content-Type"), "{out}");
    }

    #[test]
    fn an_absent_member_is_not_a_part() {
        // What `skip_serializing_if` leaves out must not come back as an empty field: a server
        // cannot tell `name=""` from "the caller did not set it".
        let out = rendered(&json!({"name": null, "kept": "yes"}), &[]);
        assert!(!out.contains("name=\"name\""), "{out}");
        assert!(out.contains("name=\"kept\""), "{out}");
    }

    #[test]
    fn the_boundary_never_appears_in_the_content() {
        // The whole reason the boundary is scanned rather than drawn: a body that happens to
        // contain it is a body the server reads as several parts, silently.
        let out = rendered(&json!({"note": "contains progeny-boundary here"}), &[]);
        assert!(out.starts_with("--progeny-boundary-1\r\n"), "{out}");
        assert!(out.ends_with("--progeny-boundary-1--\r\n"), "{out}");
        // The content is still written exactly as it was given.
        assert!(out.contains("contains progeny-boundary here"), "{out}");
    }

    #[test]
    fn the_scan_keeps_going_until_it_is_clear() {
        let value = json!({"note": "progeny-boundary progeny-boundary-1 progeny-boundary-2"});
        let (content_type, _) = body(&value, &[]).expect("an object body");
        assert!(
            content_type.ends_with("boundary=progeny-boundary-3"),
            "{content_type}"
        );
    }

    #[test]
    fn a_name_cannot_inject_a_header() {
        let out = rendered(&json!({"a\"\r\nContent-Type: text/evil": "x"}), &[]);
        assert!(
            out.contains("name=\"a\\\"Content-Type: text/evil\""),
            "{out}"
        );
        // One header line per part, and the injected one is not a line of its own.
        assert_eq!(out.matches("\r\nContent-Type:").count(), 0, "{out}");
    }

    #[test]
    fn an_array_nobody_declared_repeated_is_one_json_part() {
        // A member typed as arbitrary JSON — a degradation, or an `additionalProperties` member —
        // holds an array at run time without anything having declared it repeated. Splitting it
        // would invent a wire format from a value rather than from a declaration.
        let out = rendered(
            &json!({"payload": [1, 2]}),
            &[PartSpec {
                name: "payload",
                kind: PartKind::Json,
                repeated: false,
                content_type: None,
            }],
        );
        assert_eq!(out.matches("name=\"payload\"").count(), 1, "{out}");
        assert!(out.contains("\r\n\r\n[1,2]\r\n"), "{out}");
    }

    #[test]
    fn a_repeated_member_that_is_not_an_array_is_still_one_part() {
        // `Option<Vec<T>>` unwraps to a repeated spec, and a caller who set a single value has a
        // value that is not an array. One part is the only reading that does not drop it.
        let out = rendered(
            &json!({"tag": "solo"}),
            &[PartSpec {
                name: "tag",
                kind: PartKind::Text,
                repeated: true,
                content_type: None,
            }],
        );
        assert_eq!(out.matches("name=\"tag\"").count(), 1, "{out}");
        assert!(out.contains("\r\n\r\nsolo\r\n"), "{out}");
    }

    #[test]
    fn a_body_survives_being_written_and_read_back() {
        // The assertion that makes the writer and the reader one rule rather than two. Everything
        // comes back as text except a part that was written as JSON, because a multipart body has
        // no types — the schema says what a member is and serde is asked afterwards.
        let specs = [
            PartSpec {
                name: "file",
                kind: PartKind::File,
                repeated: false,
                content_type: None,
            },
            PartSpec {
                name: "tags",
                kind: PartKind::Text,
                repeated: true,
                content_type: None,
            },
            PartSpec {
                name: "meta",
                kind: PartKind::Json,
                repeated: false,
                content_type: None,
            },
        ];
        let original =
            json!({"file": "raw", "meta": {"a": 1}, "name": "widget", "tags": ["x", "y"]});
        let (content_type, bytes) = super::body(&original, &specs).expect("an object body");
        let boundary = super::boundary_of(&content_type).expect("a declared boundary");
        assert_eq!(super::parse(&bytes, &boundary, &specs).unwrap(), original);
    }

    #[test]
    fn a_name_that_had_to_be_escaped_comes_back_as_it_was() {
        let original = json!({"a\"b": "value"});
        let (content_type, bytes) = super::body(&original, &[]).expect("an object body");
        let boundary = super::boundary_of(&content_type).expect("a declared boundary");
        assert_eq!(super::parse(&bytes, &boundary, &[]).unwrap(), original);
    }

    #[test]
    fn a_repeated_name_the_table_never_declared_is_still_a_list() {
        // What arrived is what arrived. Dropping the first would lose a part the client sent, and
        // no table entry is going to appear at run time to say it was repeated.
        let specs = [PartSpec {
            name: "tag",
            kind: PartKind::Text,
            repeated: true,
            content_type: None,
        }];
        let (content_type, bytes) =
            super::body(&json!({"tag": ["x", "y"]}), &specs).expect("an object body");
        let boundary = super::boundary_of(&content_type).expect("a declared boundary");
        assert_eq!(
            super::parse(&bytes, &boundary, &[]).unwrap(),
            json!({"tag": ["x", "y"]})
        );
    }

    #[test]
    fn a_boundary_is_read_out_of_the_header_however_it_was_written() {
        assert_eq!(
            super::boundary_of("multipart/form-data; boundary=abc"),
            Some("abc".to_owned())
        );
        assert_eq!(
            super::boundary_of("multipart/form-data; charset=utf-8; boundary=\"a b\""),
            Some("a b".to_owned())
        );
        assert_eq!(super::boundary_of("application/json"), None);
    }

    #[test]
    fn a_body_that_is_not_an_object_has_no_parts() {
        assert!(body(&json!([1, 2]), &[]).is_none());
        assert!(body(&json!("text"), &[]).is_none());
    }
}
