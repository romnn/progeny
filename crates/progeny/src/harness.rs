//! The front end, exposed for the conformance corpus runner and the fuzz targets.
//!
//! **Not public API.** Behind the non-default `harness` feature, `#[doc(hidden)]`, and exempt
//! from semver. It exists because the round-trip property is the central claim this project rests
//! on, and a claim that only a private test can check is a claim nobody can re-run. Everything
//! here is a question about a document, never a way to reach into the model: exposing the model
//! itself would recreate the general-purpose-library surface progeny exists not to have.

mod stats;

use std::collections::BTreeMap;

use serde_json::Value;

pub use crate::resolve::Counts as Resolution;

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

/// One place where the model did not hold what the document said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Where in the document.
    pub location: String,
    /// What the difference is, as a short human sentence.
    pub detail: String,
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

/// Make a caught panic stop being fatal, for the fuzz targets.
///
/// The loader deliberately contains a panic boundary: `libyaml-safer` is a port of a C library and
/// panics on a few adversarial inputs, and progeny turns that into an ordinary rejection because "no
/// input panics the generator" is the invariant that matters. `libfuzzer-sys` installs a panic hook
/// that **aborts**, and a hook runs before unwinding — so under a fuzzer the boundary never gets to
/// act, and every such input is reported as a crash.
///
/// This replaces the hook with one that reports and returns. A panic the library catches then costs
/// a line on stderr; a panic that *escapes* still unwinds out of the fuzz target's `extern "C"`
/// entry point and still aborts, so the property under test is unchanged.
///
/// Only a fuzz target should call this. A library has no business touching a process-wide hook, and
/// this one is behind the same feature gate and the same no-semver promise as the rest of the module.
pub fn allow_caught_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            eprintln!("progeny: caught a panic and turned it into a rejection: {info}");
        }));
    });
}

/// What comparing a 3.0 document with its hand-written 3.1 equivalent found.
#[derive(Debug, Clone)]
pub struct Convergence {
    /// Where the two models disagree. Empty means the normalization did its job.
    pub differences: Vec<Difference>,
    /// Everything progeny said about either document.
    pub diagnostics: Vec<Diagnostic>,
}

impl Convergence {
    /// Whether the two documents produced the same model.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty()
    }
}

/// Check that a 3.0 document and its 3.1 equivalent produce the same model.
///
/// The two dialects are supposed to converge on one lowering, and a normalization row that is
/// merely *self-consistent* would satisfy the round-trip property while still meaning the wrong
/// thing. This is the check that the rewriting agrees with an independent statement of the same
/// API — which is why the 3.1 half of each pair is hand-written rather than generated.
///
/// The `openapi` member is excluded from the comparison: it is the one member the two documents
/// are *supposed* to disagree about, and normalization deliberately does not rewrite it.
///
/// # Errors
///
/// Returns [`RejectError`] when either document is unusable.
pub fn convergence(three_zero: &[u8], three_one: &[u8]) -> Result<Convergence, RejectError> {
    let mut ctx = Ctx::new();
    let mut model = |input: &[u8]| -> Result<Value, RejectError> {
        let loaded = load::load(input, &mut ctx)?;
        let normalized = normalize::normalize(loaded.value, &mut ctx)?;
        let parsed = doc::parse::document(normalized, &mut ctx);
        let mut value = doc::serialize::document(&parsed);
        if let Value::Object(root) = &mut value {
            root.remove("openapi");
        }
        Ok(value)
    };
    let old = model(three_zero)?;
    let new = model(three_one)?;

    let mut differences = Vec::new();
    diff(&new, &old, &JsonPointer::root(), &mut differences);
    Ok(Convergence {
        differences,
        diagnostics: ctx.into_diagnostics(),
    })
}

/// Resolve a document's references and report the accounting.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn resolution(input: &[u8]) -> Result<Resolution, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    Ok(resolve::resolve(parsed, &mut ctx).counts())
}

/// Run the front end for its diagnostics only.
///
/// The shape a fuzz target wants: it exercises every code path from bytes to model and discards
/// the result, so the property under test is "no input panics".
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn front_end(input: &[u8]) -> Result<Vec<Diagnostic>, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let _ = doc::serialize::document(&parsed);
    let _ = resolve::resolve(parsed, &mut ctx);
    Ok(ctx.into_diagnostics())
}

/// Counts over one parsed document, for the questions the corpus is the evidence base for.
///
/// Each field answers a question that decides a later design choice, and each is cheap to compute
/// once the document is a value rather than text. Adding a count here is how a design argument
/// stops being a matter of opinion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Schemas in the document.
    pub schemas: usize,
    /// `anyOf` occurrences, and the pattern each one is.
    pub any_of: AnyOfShapes,
    /// `oneOf` occurrences.
    pub one_of: usize,
    /// `oneOf`/`anyOf` occurrences carrying a discriminator.
    pub discriminated: usize,
    /// Properties that are both optional and nullable, where absent and `null` are different
    /// documents and a two-state `Option` cannot say which.
    pub optional_and_nullable: usize,
    /// Integer schemas carrying a bound, which is what would justify picking a width from bounds
    /// rather than a flat `i64`/`u64`.
    pub bounded_integers: usize,
    /// Integer schemas in total.
    pub integers: usize,
    /// Operations whose request body declares more than one media type.
    pub multi_content_operations: usize,
    /// Responses declaring headers.
    pub responses_with_headers: usize,
    /// Response headers in total.
    pub response_headers: usize,
    /// Security scheme kinds, by their `type`.
    pub security_scheme_kinds: BTreeMap<String, usize>,
    /// `$ref` strings that address another file rather than this document.
    pub external_refs: usize,
    /// `$dynamicRef` and `$dynamicAnchor` occurrences.
    pub dynamic_scoping: usize,
    /// Non-root `$id` occurrences, which change the base URI a relative reference resolves
    /// against.
    pub nested_ids: usize,
    /// `patternProperties` occurrences.
    pub pattern_properties: usize,
    /// `prefixItems` occurrences.
    pub prefix_items: usize,
    /// `const` occurrences.
    pub constants: usize,
    /// The deepest schema nesting reached.
    pub max_schema_depth: usize,
}

/// How each `anyOf` in a document is shaped.
///
/// The union policy turns on this histogram: "any combination may match" has no faithful Rust
/// type, but the overwhelming majority of real `anyOf`s are not asking for that — they are
/// emulating a nullable type or an enumeration, and those have exact translations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnyOfShapes {
    /// Occurrences in total.
    pub total: usize,
    /// `[T, {"type": "null"}]` — a nullable `T`.
    pub nullable: usize,
    /// Every branch is a `const` or a single-valued `enum` — an enumeration.
    pub constants: usize,
    /// Every branch declares a different `type` — distinguishable by shape.
    pub disjoint_types: usize,
    /// Everything else, which is where degradation lives.
    pub other: usize,
}

impl Stats {
    /// Fold another document's counts into these, so the corpus can be read as one dataset.
    ///
    /// Maxima are taken rather than summed; everything else adds.
    pub fn merge(&mut self, other: &Self) {
        self.schemas += other.schemas;
        self.any_of.total += other.any_of.total;
        self.any_of.nullable += other.any_of.nullable;
        self.any_of.constants += other.any_of.constants;
        self.any_of.disjoint_types += other.any_of.disjoint_types;
        self.any_of.other += other.any_of.other;
        self.one_of += other.one_of;
        self.discriminated += other.discriminated;
        self.optional_and_nullable += other.optional_and_nullable;
        self.bounded_integers += other.bounded_integers;
        self.integers += other.integers;
        self.multi_content_operations += other.multi_content_operations;
        self.responses_with_headers += other.responses_with_headers;
        self.response_headers += other.response_headers;
        for (kind, count) in &other.security_scheme_kinds {
            *self.security_scheme_kinds.entry(kind.clone()).or_default() += count;
        }
        self.external_refs += other.external_refs;
        self.dynamic_scoping += other.dynamic_scoping;
        self.nested_ids += other.nested_ids;
        self.pattern_properties += other.pattern_properties;
        self.prefix_items += other.prefix_items;
        self.constants += other.constants;
        self.max_schema_depth = self.max_schema_depth.max(other.max_schema_depth);
    }
}

/// Count what one document contains.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn stats(input: &[u8]) -> Result<Stats, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    Ok(stats::collect(&parsed))
}

fn diff(expected: &Value, actual: &Value, at: &JsonPointer, out: &mut Vec<Difference>) {
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
    use super::{round_trip, stats};

    const PETSTORE: &[u8] = include_bytes!("../../../corpus/specs/petstore-31.yaml");

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

    #[test]
    fn the_two_dialects_converge_on_one_model() {
        // The committed pair, so the check runs offline in `task test` as well as in the corpus.
        const OLD: &[u8] = include_bytes!("../../../corpus/convergence/dialects.3.0.yaml");
        const NEW: &[u8] = include_bytes!("../../../corpus/convergence/dialects.3.1.yaml");
        let result = super::convergence(OLD, NEW).unwrap();
        assert!(result.is_clean(), "{:#?}", result.differences);
        // The 3.1 half is already in the dialect the parser reads, so nothing about it is a
        // finding; the 3.0 half's rewriting is the documented normalization and says nothing
        // either.
        assert!(
            result.diagnostics.is_empty(),
            "{:?}",
            result
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_reference_is_accounted_for() {
        let counts = super::resolution(PETSTORE).unwrap();
        assert_eq!(
            counts.references,
            counts.resolved + counts.repaired + counts.dangling + counts.external
        );
        assert!(counts.references > 0);
    }

    #[test]
    fn the_committed_spec_can_be_counted() {
        let counted = stats(PETSTORE).unwrap();
        assert!(counted.schemas > 0);
        assert_eq!(counted.external_refs, 0);
        assert_eq!(counted.dynamic_scoping, 0);
    }
}
