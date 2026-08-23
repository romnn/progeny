//! The payload of one file part of a `multipart/form-data` body.
//!
//! This file is shipped into generated crates verbatim, and it is also compiled and unit-tested as
//! part of progeny itself — the same source in both places, so a change cannot pass the tests here
//! and break there.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "shipped into generated crates; compiled here to keep its source checked"
    )
)]

use serde::ser::SerializeMap as _;

/// One file in a `multipart/form-data` body: the bytes, and the part metadata no schema property
/// carries.
///
/// A binary member of a multipart body is this type rather than `String`, for two reasons a
/// string cannot answer: a file is bytes and a PDF is not UTF-8, and the part's `filename` and
/// `Content-Type` are real request data — servers record the filename — that the document has no
/// field for.
///
/// Between the typed body and the wire the value travels as a JSON object holding the bytes
/// base64-encoded; the multipart writer unpacks it into a real part, so base64 never reaches the
/// wire. That object is also what this type looks like inside a plain JSON payload, which is a
/// deliberate trade: a schema used both as a multipart body and as a JSON body is one type, and
/// the multipart reading is the one with a defined meaning — 3.0 defines `format: binary` for
/// exactly that position.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Upload {
    pub bytes: Vec<u8>,
    /// The part's `filename`. Absent, the writer sends `file`.
    pub filename: Option<String>,
    /// The part's own `Content-Type`. Absent, the writer falls back to what the document's
    /// `encoding` declares for the member, then to `application/octet-stream`.
    pub content_type: Option<String>,
}

impl Upload {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            filename: None,
            content_type: None,
        }
    }

    #[must_use]
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

impl From<Vec<u8>> for Upload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl serde::Serialize for Upload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut length = 1;
        if self.filename.is_some() {
            length += 1;
        }
        if self.content_type.is_some() {
            length += 1;
        }
        let mut map = serializer.serialize_map(Some(length))?;
        map.serialize_entry("bytes", &base64_encode(&self.bytes))?;
        if let Some(filename) = &self.filename {
            map.serialize_entry("filename", filename)?;
        }
        if let Some(content_type) = &self.content_type {
            map.serialize_entry("content_type", content_type)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for Upload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UploadVisitor)
    }
}

struct UploadVisitor;

impl<'de> serde::de::Visitor<'de> for UploadVisitor {
    type Value = Upload;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a file upload object, or a string holding the content")
    }

    // A bare string is what a document's example writes for a binary member, and what a payload
    // written before the object form existed holds. The string's own bytes are the content —
    // guessing base64 here would corrupt any text that happens to decode.
    fn visit_str<E>(self, content: &str) -> Result<Upload, E>
    where
        E: serde::de::Error,
    {
        Ok(Upload::new(content.as_bytes().to_vec()))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Upload, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut bytes: Option<Vec<u8>> = None;
        let mut filename: Option<String> = None;
        let mut content_type: Option<String> = None;
        while let Some(key) = access.next_key::<String>()? {
            match key.as_str() {
                "bytes" => {
                    let encoded: String = access.next_value()?;
                    bytes = Some(base64_decode(&encoded).ok_or_else(|| {
                        serde::de::Error::invalid_value(
                            serde::de::Unexpected::Str(&encoded),
                            &"base64-encoded bytes",
                        )
                    })?);
                }
                "filename" => filename = access.next_value()?,
                "content_type" => content_type = access.next_value()?,
                _ => {
                    let _: serde::de::IgnoredAny = access.next_value()?;
                }
            }
        }
        let bytes = bytes.ok_or_else(|| serde::de::Error::missing_field("bytes"))?;
        Ok(Upload {
            bytes,
            filename,
            content_type,
        })
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, written out rather than depended on: it is the only codec the
/// generated crate needs, and a dependency for thirty lines would be the tail wagging the dog.
pub fn base64_encode(bytes: &[u8]) -> String {
    // Total by masking: every index into the alphabet is six bits.
    fn glyph(value: u8) -> char {
        char::from(
            *ALPHABET
                .get(usize::from(value & 0b11_1111))
                .unwrap_or(&b'A'),
        )
    }
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut values = chunk.iter().copied();
        let Some(first) = values.next() else {
            break;
        };
        let second = values.next();
        let third = values.next();
        out.push(glyph(first >> 2));
        out.push(glyph((first & 0b11) << 4 | second.unwrap_or(0) >> 4));
        match second {
            Some(second) => {
                out.push(glyph((second & 0b1111) << 2 | third.unwrap_or(0) >> 6));
                match third {
                    Some(third) => out.push(glyph(third)),
                    None => out.push('='),
                }
            }
            None => out.push_str("=="),
        }
    }
    out
}

/// The inverse of [`base64_encode`]: standard alphabet, padding required, nothing else tolerated.
pub fn base64_decode(encoded: &str) -> Option<Vec<u8>> {
    let stripped = encoded.trim_end_matches('=');
    if !encoded.len().is_multiple_of(4) || encoded.len() - stripped.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut held: u32 = 0;
    for character in stripped.bytes() {
        let value = match character {
            b'A'..=b'Z' => u32::from(character - b'A'),
            b'a'..=b'z' => u32::from(character - b'a') + 26,
            b'0'..=b'9' => u32::from(character - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from((buffer >> held) & 0xFF).ok()?);
        }
    }
    // Padding must account for exactly the bits left over, and those bits must be zero: a
    // non-canonical encoding is somebody else's bug this should surface, not absorb.
    if buffer & ((1 << held) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{Upload, base64_decode, base64_encode};

    fn round_trips(bytes: &[u8]) {
        let encoded = base64_encode(bytes);
        assert_eq!(
            base64_decode(&encoded).as_deref(),
            Some(bytes),
            "{encoded:?}"
        );
    }

    #[test_util::test]
    fn base64_round_trips_every_remainder() {
        round_trips(b"");
        round_trips(b"f");
        round_trips(b"fo"); // spellcheck:ignore-line
        round_trips(b"foo");
        round_trips(b"foob");
        round_trips(&[0, 255, 128, 7, 63]);
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"light work."), "bGlnaHQgd29yay4=");
    }

    #[test_util::test]
    fn decode_rejects_what_no_encoder_writes() {
        assert_eq!(base64_decode("Zm9vYg="), None, "wrong length");
        assert_eq!(base64_decode("Zm 9v"), None, "whitespace");
        assert_eq!(base64_decode("Zm9vYh=="), None, "non-zero trailing bits");
        assert_eq!(base64_decode("===="), None, "too much padding");
    }

    #[test_util::test]
    fn an_upload_round_trips_through_serde() -> eyre::Result<()> {
        let upload = Upload::new(b"raw \xFF bytes".to_vec())
            .with_filename("report.pdf")
            .with_content_type("application/pdf");
        let value = serde_json::to_value(&upload)?;
        assert_eq!(
            value,
            serde_json::json!({
                "bytes": base64_encode(b"raw \xFF bytes"),
                "filename": "report.pdf",
                "content_type": "application/pdf",
            })
        );
        let back: Upload = serde_json::from_value(value)?;
        assert_eq!(back, upload);
        Ok(())
    }

    #[test_util::test]
    fn a_bare_string_is_content_with_no_metadata() -> eyre::Result<()> {
        let upload: Upload = serde_json::from_value(serde_json::json!("hello"))?;
        assert_eq!(upload, Upload::new(b"hello".to_vec()));
        Ok(())
    }
}
