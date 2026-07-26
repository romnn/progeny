//! The edge of the pipeline: bytes to `serde_json::Value`.
//!
//! This is the only place that touches source syntax. Everything downstream sees a
//! `serde_json::Value`, so the two source formats converge here rather than in two parsers.
//!
//! Losslessness is defined on the loaded value, not on the source text: key order and
//! whitespace carry no meaning, so the round-trip property compares values. Number *literals*
//! do carry meaning — `1` and `1.0` are different defaults to render — so they are preserved
//! exactly, which is why the value type enables arbitrary-precision numbers.

mod yaml;

use serde_json::Value;

use crate::diag::{Ctx, RejectError, RejectKind};

/// Which syntax the document was written in.
///
/// Recorded rather than inferred from a filename because filenames lie: one corpus document
/// is served as YAML from an extensionless URL and cached as `.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceFormat {
    Json,
    Yaml,
}

/// A document as loaded, before any interpretation.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) value: Value,
    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read by the corpus harness, which is feature-gated"
        )
    )]
    pub(crate) format: SourceFormat,
}

/// Load a document, trying JSON and falling back to YAML.
///
/// The order is by cost, not by preference: JSON fails on the first byte of a YAML document,
/// while YAML would accept either and then have to answer the harder questions (which scalars
/// are numbers, which keys are strings) that a JSON parser answers by grammar.
pub(crate) fn load(input: &[u8], ctx: &mut Ctx) -> Result<Loaded, RejectError> {
    // A byte-order mark is legal in YAML and rejected by JSON; either way it is an encoding
    // artifact and not document content.
    let input = input.strip_prefix("\u{feff}".as_bytes()).unwrap_or(input);

    let json_error = match serde_json::from_slice::<Value>(input) {
        Ok(value) => {
            return Ok(Loaded {
                value,
                format: SourceFormat::Json,
            });
        }
        Err(error) => error,
    };

    let text = std::str::from_utf8(input).map_err(|error| {
        RejectError::new(
            RejectKind::Unparsable,
            format!(
                "the document is neither JSON ({json_error}) nor UTF-8 text that could be YAML ({error})"
            ),
        )
    })?;

    match yaml::load(text, ctx) {
        Ok(value) => Ok(Loaded {
            value,
            format: SourceFormat::Yaml,
        }),
        Err(yaml_error) if yaml_error.kind() == RejectKind::Unparsable => Err(RejectError::new(
            RejectKind::Unparsable,
            format!(
                "the document parses as neither JSON ({json_error}) nor YAML ({})",
                yaml_error.detail()
            ),
        )),
        // A YAML-specific rejection (a mapping key that is not a scalar) is a real finding
        // about a real YAML document, not a format guess gone wrong. Report it as-is.
        Err(yaml_error) => Err(yaml_error),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{SourceFormat, load};
    use crate::diag::{Ctx, RejectKind};

    fn loaded(input: &str) -> (serde_json::Value, SourceFormat) {
        let mut ctx = Ctx::new();
        let out = load(input.as_bytes(), &mut ctx).unwrap();
        (out.value, out.format)
    }

    #[test]
    fn json_is_recognized_as_json() {
        let (value, format) = loaded(r#"{"openapi": "3.1.0"}"#);
        assert_eq!(format, SourceFormat::Json);
        assert_eq!(value, json!({"openapi": "3.1.0"}));
    }

    #[test]
    fn yaml_is_recognized_as_yaml() {
        let (value, format) = loaded("openapi: 3.1.0\ninfo:\n  title: x\n");
        assert_eq!(format, SourceFormat::Yaml);
        assert_eq!(value, json!({"openapi": "3.1.0", "info": {"title": "x"}}));
    }

    #[test]
    fn a_byte_order_mark_is_not_document_content() {
        let (value, format) = loaded("\u{feff}{\"a\": 1}");
        assert_eq!(format, SourceFormat::Json);
        assert_eq!(value, json!({"a": 1}));
    }

    #[test]
    fn neither_format_reports_both_errors() {
        let mut ctx = Ctx::new();
        let error = load(b"{\"a\": [", &mut ctx).unwrap_err();
        assert_eq!(error.kind(), RejectKind::Unparsable);
        assert!(error.detail().contains("JSON"), "{error}");
        assert!(error.detail().contains("YAML"), "{error}");
    }

    #[test]
    fn invalid_utf8_is_rejected_rather_than_lossily_decoded() {
        let mut ctx = Ctx::new();
        let error = load(&[0x6f, 0x6b, 0x3a, 0x20, 0xff], &mut ctx).unwrap_err();
        assert_eq!(error.kind(), RejectKind::Unparsable);
    }
}
