//! The round-trip property: the model holds the document exactly, and can prove it.

use serde_json::Value;

use super::{Difference, Resolution};
use crate::diag::{Ctx, Diagnostic, JsonPointer, RejectError};
use crate::{doc, load, normalize, resolve};

/// What a round trip through the model found.
#[derive(Debug, Clone)]
pub struct RoundTrip {
    /// The loaded document after dialect normalization — the value the model is compared to.
    ///
    /// Losslessness is defined against this rather than against the source text: JSON object
    /// member order and whitespace carry no meaning, and a 3.0 document is compared after the
    /// documented rewriting rather than before it.
    pub normalized: Value,
    /// The value the model serializes back to.
    pub reserialized: Value,
    /// Where the two differ. Empty means the model held the document exactly.
    pub differences: Vec<Difference>,
    /// Everything progeny had to say about the document.
    pub diagnostics: Vec<Diagnostic>,
    /// How many schemas the document contained.
    pub schema_count: usize,
    /// Which syntax the document was written in.
    pub yaml: bool,
    /// The `openapi` version the document declared, exactly as written.
    pub declared_version: String,
    /// What resolving the document's references found.
    pub resolution: Resolution,
}

impl RoundTrip {
    /// Whether the model held the document exactly.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty()
    }
}

/// Load, normalize, parse, serialize, and compare.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn round_trip(input: &[u8]) -> Result<RoundTrip, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let yaml = loaded.format == load::SourceFormat::Yaml;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let expected = normalized.value().clone();
    let declared_version = normalized.version().text.clone();

    let parsed = doc::parse::document(normalized, &mut ctx);
    let reserialized = doc::serialize::document(&parsed);
    let schema_count = parsed.schemas.len();
    // Resolution runs after the comparison value has been taken, because it consumes the parsed
    // document — and the round trip is a property of the parsed model, not of the resolved one.
    let resolution = resolve::resolve(parsed, &mut ctx).counts();

    let mut differences = Vec::new();
    diff(
        &expected,
        &reserialized,
        &JsonPointer::root(),
        &mut differences,
    );

    Ok(RoundTrip {
        normalized: expected,
        reserialized,
        differences,
        diagnostics: ctx.into_diagnostics(),
        schema_count,
        yaml,
        declared_version,
        resolution,
    })
}

pub(super) fn diff(expected: &Value, actual: &Value, at: &JsonPointer, out: &mut Vec<Difference>) {
    // A handful of differences is enough to diagnose a defect; thousands are noise that hides it.
    const LIMIT: usize = 20;
    if out.len() >= LIMIT {
        return;
    }
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                match actual.get(key) {
                    Some(other) => diff(value, other, &at.child(key), out),
                    None => out.push(Difference {
                        location: at.child(key).to_string(),
                        detail: "the model dropped this member".to_owned(),
                    }),
                }
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    out.push(Difference {
                        location: at.child(key).to_string(),
                        detail: "the model invented this member".to_owned(),
                    });
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                out.push(Difference {
                    location: at.to_string(),
                    detail: format!(
                        "the document has {} elements, the model has {}",
                        expected.len(),
                        actual.len()
                    ),
                });
                return;
            }
            for (index, (value, other)) in expected.iter().zip(actual).enumerate() {
                diff(value, other, &at.child(index.to_string()), out);
            }
        }
        (expected, actual) if expected == actual => {}
        (expected, actual) => out.push(Difference {
            location: at.to_string(),
            detail: format!("the document says {expected}, the model says {actual}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::round_trip;

    const PETSTORE: &[u8] = include_bytes!("../../../../corpus/specs/petstore-31.yaml");

    #[test]
    fn the_committed_spec_round_trips_exactly() {
        let result = round_trip(PETSTORE).unwrap();
        assert!(result.is_clean(), "{:#?}", result.differences);
        assert!(result.yaml);
        assert!(result.schema_count > 0);
    }

    #[test]
    fn a_dropped_member_is_reported_with_its_location() {
        let mut differences = Vec::new();
        super::diff(
            &serde_json::json!({"a": {"b": 1}}),
            &serde_json::json!({"a": {}}),
            &crate::JsonPointer::root(),
            &mut differences,
        );
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].location, "/a/b");
    }

    #[test]
    fn an_invented_member_is_reported_too() {
        let mut differences = Vec::new();
        super::diff(
            &serde_json::json!({}),
            &serde_json::json!({"a": 1}),
            &crate::JsonPointer::root(),
            &mut differences,
        );
        assert_eq!(differences.len(), 1);
        assert!(differences[0].detail.contains("invented"));
    }
}
