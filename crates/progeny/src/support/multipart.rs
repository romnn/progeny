//! `multipart/form-data` bodies.
//!
//! Shipped into generated crates and compiled here, so the format is unit-tested in the progeny
//! workspace rather than only in whatever a consumer happens to generate.
//!
//! The body is assembled from a `serde_json::Value` and a table of part specifications, rather than
//! from code unrolled per operation. Two reasons, and they are the same two as the style table: one
//! non-generic body instead of one per operation is compile time a consumer does not pay, and one
//! place where the format's rules live is one place they can be wrong.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "shipped into generated crates; compiled here to keep its source checked"
    )
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
    /// The member's Rust type is `Upload` for a multipart body's own members — bytes plus the
    /// part metadata, arriving here as the object its serializer writes — and `String` where a
    /// binary property sits deeper in the body, because inside a JSON payload it *is* a string.
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
    let declared = content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        (name.trim().eq_ignore_ascii_case("boundary"))
            .then(|| value.trim().trim_matches('"').to_owned())
    })?;
    // RFC 2046's grammar: one to seventy characters from its `bchars` set. The length bound
    // is also this parser's cost bound — hyper accepts header blocks past 400 KB, and a
    // caller-sized delimiter would make the scan below quadratic in something an attacker
    // chooses freely.
    if declared.is_empty() || declared.len() > 70 {
        return None;
    }
    let legal = declared.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'\''
                    | b'('
                    | b')'
                    | b'+'
                    | b'_'
                    | b','
                    | b'-'
                    | b'.'
                    | b'/'
                    | b':'
                    | b'='
                    | b'?'
                    | b' '
            )
    });
    legal.then_some(declared)
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
/// Returns [`ParseError`] when the body is not well-formed multipart for the boundary given.
pub fn parse(body: &[u8], boundary: &str, specs: &[PartSpec]) -> Result<Value, ParseError> {
    let delimiter = format!("--{boundary}");
    let mut members = serde_json::Map::new();
    for section in split_on(body, delimiter.as_bytes()).skip(1) {
        // The trailer after the closing delimiter is `--`, and anything after that is epilogue.
        if section.starts_with(b"--") {
            break;
        }
        let Some(headers_end) = find(section, b"\r\n\r\n") else {
            return Err(ParseError::MissingHeaderBlock);
        };
        let headers = String::from_utf8_lossy(section.get(..headers_end).unwrap_or_default());
        let content = section.get(headers_end + 4..).unwrap_or_default();
        // A part whose final CRLF is missing keeps its content: defaulting to empty read a
        // slightly non-conforming body as an empty member — silent data loss, where this
        // module's rule is to read faithfully or refuse out loud.
        let content = content.strip_suffix(b"\r\n").unwrap_or(content);
        let Some(name) = disposition_name(&headers) else {
            return Err(ParseError::MissingName);
        };
        let kind = specs
            .iter()
            .find(|spec| spec.name == name)
            .map_or(PartKind::Text, |spec| spec.kind);
        let repeated = specs
            .iter()
            .find(|spec| spec.name == name)
            .is_some_and(|spec| spec.repeated);
        let value = match kind {
            PartKind::Json => {
                let text = String::from_utf8_lossy(content).into_owned();
                serde_json::from_str(&text).map_err(ParseError::InvalidJson)?
            }
            PartKind::Text => Value::String(String::from_utf8_lossy(content).into_owned()),
            // The object form an `Upload` deserializes from: the bytes kept intact — a file is
            // not UTF-8 and a lossy string would corrupt it — and the part metadata the wire
            // actually carried, so a handler sees the filename the client sent.
            PartKind::File => {
                let mut members = serde_json::Map::new();
                members.insert(
                    "bytes".to_owned(),
                    Value::String(super::base64_encode(content)),
                );
                if let Some(filename) = disposition_filename(&headers) {
                    members.insert("filename".to_owned(), Value::String(filename));
                }
                if let Some(content_type) = content_type_of(&headers) {
                    members.insert("content_type".to_owned(), Value::String(content_type));
                }
                Value::Object(members)
            }
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

/// Why a multipart request could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// A part did not separate its headers from its body.
    #[error("a multipart section has no header block")]
    MissingHeaderBlock,
    /// A part did not declare the member name it belongs to.
    #[error("a multipart section declares no name")]
    MissingName,
    /// A part declared as JSON did not contain JSON.
    #[error("a multipart JSON part is invalid")]
    InvalidJson(#[source] serde_json::Error),
}

/// The `name` of a section, from its `Content-Disposition`.
fn disposition_name(headers: &str) -> Option<String> {
    disposition_parameter(headers, "name")
}

/// The `filename` parameter of the part's disposition, when the sender included one.
fn disposition_filename(headers: &str) -> Option<String> {
    disposition_parameter(headers, "filename")
}

/// One parameter of the part's `Content-Disposition` line, matched by its whole key.
///
/// The line is tokenized into `;`-separated parameters with quoting respected, and each key is
/// compared exactly. A substring search over the whole line found the `name="` *inside*
/// `filename="` whenever the sender wrote the filename first — a legal ordering — and filed the
/// part's bytes under its filename.
fn disposition_parameter(headers: &str, key: &str) -> Option<String> {
    let line = headers.lines().find(|line| {
        line.split(':')
            .next()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("content-disposition"))
    })?;
    let (_, parameters) = line.split_once(':')?;
    let mut split: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for character in parameters.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quotes => {
                current.push(character);
                escaped = true;
            }
            '"' => {
                current.push('"');
                in_quotes = !in_quotes;
            }
            ';' if !in_quotes => split.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    split.push(current);
    for parameter in split {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let value = value.trim();
        let Some(rest) = value.strip_prefix('"') else {
            // An unquoted value ends at the parameter, which the tokenizer already bounded.
            return Some(value.to_owned());
        };
        // Undo the escaping the writer applies, so a name with a quote in it survives the
        // round trip.
        let mut out = String::new();
        let mut characters = rest.chars();
        while let Some(character) = characters.next() {
            match character {
                '"' => return Some(out),
                '\\' => out.push(characters.next()?),
                other => out.push(other),
            }
        }
        return None;
    }
    None
}

/// The part's own `Content-Type` header, when the sender wrote one.
fn content_type_of(headers: &str) -> Option<String> {
    let line = headers.lines().find(|line| {
        line.split(':')
            .next()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("content-type"))
    })?;
    let (_, value) = line.split_once(':')?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The body split on a delimiter, each piece with its leading `\r\n` removed.
fn split_on<'a>(body: &'a [u8], delimiter: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    let mut rest = Some(body);
    let mut first = true;
    std::iter::from_fn(move || {
        let current = rest?;
        // The tail after the last delimiter is still a piece, which is why the `None` arm yields
        // rather than ending the iterator: a body whose final boundary is missing its trailing
        // `--` would otherwise lose its last part silently.
        let piece = if let Some(at) = find_at_line_start(current, delimiter, first) {
            rest = current.get(at + delimiter.len()..);
            current.get(..at).unwrap_or_default()
        } else {
            rest = None;
            current
        };
        first = false;
        Some(piece.strip_prefix(b"\r\n").unwrap_or(piece))
    })
}

/// The first occurrence of the delimiter at the start of a line.
///
/// RFC 2046 requires the boundary delimiter at the beginning of a line, and matching it
/// mid-line let text that merely *contains* the delimiter — a part name, a body a foreign
/// client failed to scan — re-frame everything after it. Position 0 counts as a line start
/// only for the body's own first byte.
fn find_at_line_start(haystack: &[u8], delimiter: &[u8], first: bool) -> Option<usize> {
    if first && haystack.starts_with(delimiter) {
        return Some(0);
    }
    let mut from = 0;
    while let Some(at) = find(haystack.get(from..)?, b"\r\n") {
        let start = from + at + 2;
        if haystack.get(start..)?.starts_with(delimiter) {
            return Some(start);
        }
        from = start;
    }
    None
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
    // The object form is what an `Upload` serializes to: the content as real bytes, and the part
    // metadata — `filename`, the part's own `Content-Type` — that no schema property carries.
    // Written without a let-chain: this file ships into generated crates, whose manifests say
    // edition 2021, where the chain does not parse.
    let upload = match value {
        Value::Object(members) => match members.get("bytes") {
            Some(Value::String(encoded)) => {
                super::base64_decode(encoded).map(|bytes| (members, bytes))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some((members, bytes)) = upload {
        let field = |name: &str| match members.get(name) {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };
        return Part {
            filename: Some(field("filename").unwrap_or_else(|| "file".to_owned())),
            content_type: Some(
                field("content_type")
                    .or_else(|| content_type.map(ToOwned::to_owned))
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
            ),
            body: bytes,
        };
    }
    Part {
        // A `filename` is what makes a server treat the part as an upload rather than a field, and
        // a plain string carries none. Absent would be faithful and useless; this is the
        // conventional stand-in.
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
    // The scan covers everything [`write`] will put between two delimiters, *as written*: the
    // body, and the name, filename and content type in their serialized forms — `quoted` drops
    // CR and LF, so a filename that hides the stem behind a line break collides only in its
    // quoted spelling, and the raw form is the wrong thing to ask. The part name is caller
    // data too, whenever the body is a map or captures unknown members.
    let collides = |candidate: &str| {
        parts.iter().any(|(name, part)| {
            contains(&part.body, candidate.as_bytes())
                || quoted(name).contains(candidate)
                || part
                    .filename
                    .as_deref()
                    .is_some_and(|filename| quoted(filename).contains(candidate))
                || part
                    .content_type
                    .as_deref()
                    .is_some_and(|content_type| header_safe(content_type).contains(candidate))
        })
    };
    // One scan for the stem picks the starting suffix: one past the largest `-N` any
    // occurrence carries, so content that enumerates `progeny-boundary-1 ... -100000` costs
    // one pass rather than one pass per candidate. The loop below stays as the correctness
    // argument — the arithmetic is an optimization, never the proof — and settles immediately
    // for any input whose suffixes parse as decimals.
    let mut attempt = 0u32;
    let mut candidate = STEM.to_owned();
    if collides(&candidate) {
        for (name, part) in parts {
            for text in [
                String::from_utf8_lossy(&part.body).into_owned(),
                quoted(name),
                part.filename.as_deref().map(quoted).unwrap_or_default(),
                part.content_type
                    .as_deref()
                    .map(header_safe)
                    .unwrap_or_default(),
            ] {
                for (position, _) in text.match_indices(STEM) {
                    let digits: String = text
                        .get(position + STEM.len()..)
                        .unwrap_or_default()
                        .strip_prefix('-')
                        .unwrap_or_default()
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    let numbered = digits.parse::<u32>().unwrap_or(0);
                    attempt = attempt.max(numbered.saturating_add(1));
                }
            }
        }
        attempt = attempt.max(1);
        candidate = format!("{STEM}-{attempt}");
        while collides(&candidate) {
            attempt = attempt.saturating_add(1);
            candidate = format!("{STEM}-{attempt}");
        }
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
        let mut headers = crlf(&indoc::formatdoc! {r#"
            --{boundary}
            Content-Disposition: form-data; name="{}""#,
            quoted(name)
        });
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

fn crlf(text: &str) -> String {
    text.replace('\n', "\r\n")
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
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::json;

    use super::{PartKind, PartSpec, body, crlf};

    fn rendered(value: &serde_json::Value, specs: &[PartSpec]) -> eyre::Result<String> {
        let (_, bytes) =
            body(value, specs).ok_or_eyre("test fixture should produce a multipart body")?;
        Ok(String::from_utf8(bytes)?)
    }

    fn framed(text: &str) -> String {
        const LINE_BREAK: &str = "\r\n";
        format!("{LINE_BREAK}{LINE_BREAK}{text}{LINE_BREAK}")
    }

    fn on_new_line(text: &str) -> String {
        const LINE_BREAK: &str = "\r\n";
        format!("{LINE_BREAK}{text}")
    }

    #[test_util::test]
    fn a_scalar_member_is_a_named_field() {
        let out = rendered(&json!({"name": "widget"}), &[])?;
        assert_eq!(
            out,
            crlf(indoc::indoc! {r#"
                --progeny-boundary
                Content-Disposition: form-data; name="name"

                widget
                --progeny-boundary--
            "#})
        );
    }

    #[test_util::test]
    fn a_binary_member_carries_a_filename_and_a_content_type() {
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        let out = rendered(&json!({"file": "raw bytes"}), &specs)?;
        assert!(out.contains("name=\"file\"; filename=\"file\""), "{out}");
        assert!(
            out.contains("Content-Type: application/octet-stream"),
            "{out}"
        );
        assert!(out.contains(&framed("raw bytes")), "{out}");
    }

    #[test_util::test]
    fn a_declared_content_type_wins_over_the_default() {
        let specs = [PartSpec {
            name: "metadata",
            kind: PartKind::Json,
            repeated: false,
            content_type: Some("application/json"),
        }];
        let out = rendered(&json!({"metadata": {"a": 1}}), &specs)?;
        assert!(out.contains("Content-Type: application/json"), "{out}");
        assert!(out.contains(&framed(r#"{"a":1}"#)), "{out}");
    }

    #[test_util::test]
    fn a_structured_member_nobody_described_is_json() {
        // The inference, and the reason it is not `Text`: `[1,2]` has no faithful text form, and
        // `to_string` on the `Value` would produce the JSON anyway without saying so in a header.
        let out = rendered(&json!({"tags": ["a", "b"]}), &[])?;
        assert!(out.contains("Content-Type: application/json"), "{out}");
        assert!(out.contains(&framed(r#"["a","b"]"#)), "{out}");
    }

    #[test_util::test]
    fn an_array_of_files_is_one_part_each() {
        // `anthropic` writes exactly this: `files` as an array of `format: binary` strings.
        let specs = [PartSpec {
            name: "files",
            kind: PartKind::File,
            repeated: true,
            content_type: None,
        }];
        let out = rendered(&json!({"files": ["first", "second"]}), &specs)?;
        assert_eq!(out.matches("name=\"files\"; filename=\"file\"").count(), 2);
        assert!(out.contains(&framed("first")), "{out}");
        assert!(out.contains(&framed("second")), "{out}");
    }

    #[test_util::test]
    fn an_array_of_scalars_is_one_part_each() {
        let out = rendered(
            &json!({"tag": ["a", "b"]}),
            &[PartSpec {
                name: "tag",
                kind: PartKind::Text,
                repeated: true,
                content_type: None,
            }],
        )?;
        assert_eq!(out.matches("name=\"tag\"").count(), 2);
        assert!(!out.contains("Content-Type"), "{out}");
    }

    #[test_util::test]
    fn an_absent_member_is_not_a_part() {
        // What `skip_serializing_if` leaves out must not come back as an empty field: a server
        // cannot tell `name=""` from "the caller did not set it".
        let out = rendered(&json!({"name": null, "kept": "yes"}), &[])?;
        assert!(!out.contains("name=\"name\""), "{out}");
        assert!(out.contains("name=\"kept\""), "{out}");
    }

    #[test_util::test]
    fn the_boundary_never_appears_in_the_content() {
        // The whole reason the boundary is scanned rather than drawn: a body that happens to
        // contain it is a body the server reads as several parts, silently.
        let out = rendered(&json!({"note": "contains progeny-boundary here"}), &[])?;
        assert!(out.starts_with("--progeny-boundary-1\r\n"), "{out}");
        assert!(out.ends_with("--progeny-boundary-1--\r\n"), "{out}");
        // The content is still written exactly as it was given.
        assert!(out.contains("contains progeny-boundary here"), "{out}");
    }

    #[test_util::test]
    fn the_scan_keeps_going_until_it_is_clear() {
        let value = json!({"note": "progeny-boundary progeny-boundary-1 progeny-boundary-2"});
        let (content_type, _) =
            body(&value, &[]).ok_or_eyre("test fixture should contain this value")?;
        assert!(
            content_type.ends_with("boundary=progeny-boundary-3"),
            "{content_type}"
        );
    }

    #[test_util::test]
    fn a_name_cannot_inject_a_header() {
        let malicious = crlf(indoc::indoc! {r#"
            a"
            Content-Type: text/evil"#});
        let value = serde_json::Value::Object(
            [(malicious, serde_json::Value::String("x".to_owned()))]
                .into_iter()
                .collect(),
        );
        let out = rendered(&value, &[])?;
        assert!(
            out.contains("name=\"a\\\"Content-Type: text/evil\""),
            "{out}"
        );
        // One header line per part, and the injected one is not a line of its own.
        assert_eq!(
            out.matches(&on_new_line("Content-Type:")).count(),
            0,
            "{out}"
        );
    }

    #[test_util::test]
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
        )?;
        assert_eq!(out.matches("name=\"payload\"").count(), 1, "{out}");
        assert!(out.contains(&framed("[1,2]")), "{out}");
    }

    #[test_util::test]
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
        )?;
        assert_eq!(out.matches("name=\"tag\"").count(), 1, "{out}");
        assert!(out.contains(&framed("solo")), "{out}");
    }

    #[test_util::test]
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
        let original = json!({
            "file": {"bytes": "cmF3", "content_type": "application/pdf", "filename": "report.pdf"},
            "meta": {"a": 1},
            "name": "widget",
            "tags": ["x", "y"],
        });
        let (content_type, bytes) =
            super::body(&original, &specs).ok_or_eyre("test fixture should contain this value")?;
        let boundary = super::boundary_of(&content_type)
            .ok_or_eyre("test fixture should contain this value")?;
        assert_eq!(super::parse(&bytes, &boundary, &specs)?, original);
    }

    #[test_util::test]
    fn a_boundary_never_hides_in_a_filename_the_caller_chose() {
        // The filename rides the part's header block, so a boundary contained in it would split
        // the framing exactly as one in a body would — and the filename is caller data.
        let value = serde_json::json!({
            "file": {
                "bytes": "aGVsbG8=",
                "filename": "progeny-boundary",
            },
        });
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        let (content_type, bytes) = super::body(&value, &specs).ok_or_eyre("a form body")?;
        let boundary = super::boundary_of(&content_type).ok_or_eyre("a declared boundary")?;
        assert!(
            !"progeny-boundary".contains(&boundary),
            "the boundary `{boundary}` hides in the filename"
        );
        let parsed = super::parse(&bytes, &boundary, &specs)?;
        let member = parsed
            .get("file")
            .and_then(|member| member.get("bytes"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(member, Some("aGVsbG8="));
    }

    #[test_util::test]
    fn a_boundary_never_hides_in_a_part_name_the_caller_chose() {
        // With a map-typed body the part names are the caller's keys, so a name can carry the
        // stem into the header block; unscanned, the delimiter landed inside a disposition
        // line and every later member was dropped without an error.
        let value = json!({
            "x-progeny-boundary-y": "first",
            "note": "second",
        });
        let (content_type, bytes) = super::body(&value, &[]).ok_or_eyre("a form body")?;
        let boundary = super::boundary_of(&content_type).ok_or_eyre("a declared boundary")?;
        assert!(
            !"x-progeny-boundary-y".contains(&boundary),
            "the boundary `{boundary}` hides in the part name"
        );
        let parsed = super::parse(&bytes, &boundary, &[])?;
        assert_eq!(
            parsed.get("note").and_then(serde_json::Value::as_str),
            Some("second")
        );
    }

    #[test_util::test]
    fn the_scan_reads_metadata_as_it_is_written_not_as_it_was_given() {
        // `quoted` drops CR and LF from a filename, so a stem split by a line break exists
        // only in the serialized spelling — which is the spelling the wire carries and the one
        // the scan has to ask about.
        let value = json!({
            "file": {
                "bytes": "aGVsbG8=",
                "filename": "progeny-\r\nboundary",
            },
        });
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        let (content_type, bytes) = super::body(&value, &specs).ok_or_eyre("a form body")?;
        let boundary = super::boundary_of(&content_type).ok_or_eyre("a declared boundary")?;
        assert!(
            !"progeny-boundary".contains(&boundary),
            "the boundary `{boundary}` hides in the quoted filename"
        );
        let parsed = super::parse(&bytes, &boundary, &specs)?;
        let member = parsed
            .get("file")
            .and_then(|member| member.get("bytes"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(member, Some("aGVsbG8="));
    }

    #[test_util::test]
    fn a_filename_written_before_the_name_does_not_claim_its_part() {
        // The parameter order in a disposition line is the sender's choice; a substring search
        // read the `name="` inside `filename="` and filed the bytes under the filename.
        let body = b"--b\r\nContent-Disposition: form-data; filename=\"signature\"; name=\"file\"\r\n\r\ncontent\r\n--b--\r\n";
        let parsed = super::parse(body, "b", &[])?;
        assert_eq!(
            parsed.get("file").and_then(serde_json::Value::as_str),
            Some("content")
        );
        assert!(parsed.get("signature").is_none(), "{parsed:?}");
    }

    #[test_util::test]
    fn a_part_missing_its_final_line_break_keeps_its_content() {
        // A slightly non-conforming body read as an *empty* member is silent data loss; the
        // content stays, trailer text and all.
        let body = b"--b\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nsecret--b--\r\n";
        let parsed = super::parse(body, "b", &[])?;
        let note = parsed
            .get("note")
            .and_then(serde_json::Value::as_str)
            .ok_or_eyre("the member survives")?;
        assert!(note.starts_with("secret"), "{note:?}");
    }

    #[test_util::test]
    fn a_delimiter_mid_line_does_not_reframe_the_body() {
        // RFC 2046 requires the delimiter at a line start; matched mid-line, content that
        // merely contains it swallowed every later part.
        let body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nx--b--y\r\n--b\r\nContent-Disposition: form-data; name=\"c\"\r\n\r\nd\r\n--b--\r\n";
        let parsed = super::parse(body, "b", &[])?;
        assert_eq!(
            parsed.get("a").and_then(serde_json::Value::as_str),
            Some("x--b--y")
        );
        assert_eq!(
            parsed.get("c").and_then(serde_json::Value::as_str),
            Some("d")
        );
    }

    #[test_util::test]
    fn a_boundary_outside_the_grammar_is_refused() {
        // RFC 2046 bounds the boundary at seventy characters from a fixed set; the bound is
        // also what keeps the parser's scan linear in the body alone.
        let long = "a".repeat(71);
        assert_eq!(
            super::boundary_of(&format!("multipart/form-data; boundary={long}")),
            None
        );
        assert_eq!(
            super::boundary_of("multipart/form-data; boundary=a\x07b"),
            None
        );
        assert_eq!(super::boundary_of("multipart/form-data; boundary="), None);
        assert_eq!(
            super::boundary_of("multipart/form-data; boundary=ok-1.2:3"),
            Some("ok-1.2:3".to_owned())
        );
    }

    #[test_util::test]
    fn an_enumerating_body_settles_the_boundary_in_one_scan() {
        // Content that lists `progeny-boundary progeny-boundary-1 ... -3` used to cost one
        // full scan per candidate; the suffix now comes from a single pass.
        let value = json!({
            "note": "progeny-boundary progeny-boundary-1 progeny-boundary-2 progeny-boundary-3",
        });
        let (content_type, bytes) = super::body(&value, &[]).ok_or_eyre("a form body")?;
        let boundary = super::boundary_of(&content_type).ok_or_eyre("a declared boundary")?;
        assert_eq!(boundary, "progeny-boundary-4");
        let parsed = super::parse(&bytes, &boundary, &[])?;
        assert!(parsed.get("note").is_some(), "{parsed:?}");
    }

    #[test_util::test]
    fn an_upload_object_becomes_a_part_with_its_own_metadata() -> eyre::Result<()> {
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        // "raw \xFF bytes" — content no UTF-8 string could carry.
        let encoded = "cmF3IP8gYnl0ZXM=";
        let (_, bytes) = super::body(
            &json!({"file": {"bytes": encoded, "filename": "report.pdf", "content_type": "application/pdf"}}),
            &specs,
        )
        .ok_or_eyre("test fixture should contain this value")?;
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(
            rendered.contains("name=\"file\"; filename=\"report.pdf\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("Content-Type: application/pdf"),
            "{rendered}"
        );
        let raw = b"raw \xFF bytes";
        assert!(
            bytes.windows(raw.len()).any(|window| window == raw),
            "the part body is the decoded bytes, not the base64 text"
        );
        Ok(())
    }

    #[test_util::test]
    fn a_file_part_from_a_foreign_body_keeps_its_bytes_and_metadata() -> eyre::Result<()> {
        // A body some other client wrote: the reader owes the handler the real bytes and the
        // metadata the wire carried, without inventing what the wire omitted.
        let specs = [PartSpec {
            name: "file",
            kind: PartKind::File,
            repeated: false,
            content_type: None,
        }];
        let body =
            b"--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\ncontent\r\n--b--\r\n";
        assert_eq!(
            super::parse(body, "b", &specs)?,
            json!({"file": {"bytes": "Y29udGVudA=="}})
        );
        Ok(())
    }

    #[test_util::test]
    fn a_name_that_had_to_be_escaped_comes_back_as_it_was() {
        let original = json!({"a\"b": "value"});
        let (content_type, bytes) =
            super::body(&original, &[]).ok_or_eyre("test fixture should contain this value")?;
        let boundary = super::boundary_of(&content_type)
            .ok_or_eyre("test fixture should contain this value")?;
        assert_eq!(super::parse(&bytes, &boundary, &[])?, original);
    }

    #[test_util::test]
    fn a_repeated_name_the_table_never_declared_is_still_a_list() {
        // What arrived is what arrived. Dropping the first would lose a part the client sent, and
        // no table entry is going to appear at run time to say it was repeated.
        let specs = [PartSpec {
            name: "tag",
            kind: PartKind::Text,
            repeated: true,
            content_type: None,
        }];
        let (content_type, bytes) = super::body(&json!({"tag": ["x", "y"]}), &specs)
            .ok_or_eyre("test fixture should contain this value")?;
        let boundary = super::boundary_of(&content_type)
            .ok_or_eyre("test fixture should contain this value")?;
        assert_eq!(
            super::parse(&bytes, &boundary, &[])?,
            json!({"tag": ["x", "y"]})
        );
    }

    #[test_util::test]
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

    #[test_util::test]
    fn a_body_that_is_not_an_object_has_no_parts() {
        assert!(body(&json!([1, 2]), &[]).is_none());
        assert!(body(&json!("text"), &[]).is_none());
    }
}
