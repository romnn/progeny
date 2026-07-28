//! The two dialects converge on one lowering, and the generated crate is the proof.

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use serde_json::Value;

use super::Difference;
use super::trip::diff;
use crate::diag::{Ctx, Diagnostic, JsonPointer, RejectError};
use crate::{api, contract, doc, load, normalize, render, resolve, shape};

/// What comparing a 3.0 document with its hand-written 3.1 equivalent found.
#[derive(Debug, Clone)]
pub struct Convergence {
    /// Where the two models disagree. Empty means the normalization did its job.
    pub differences: Vec<Difference>,
    /// Where the two *renderings* disagree, by file.
    ///
    /// The model half cannot see a difference the model does not carry — two shapes that serialize
    /// alike but name their type differently, or a rendering decision keyed on something
    /// normalization left alone. What the two dialects have to agree on is the generated crate,
    /// because that is the artifact a caller receives.
    pub output: Vec<Difference>,
    /// A degradation one dialect suffered and the other did not.
    ///
    /// The two halves are supposed to give up the same things in the same places. A `Degrade` or a
    /// `Warn` on one side alone means the dialects did not converge even where the source happens
    /// to match — and the *shared* ones cannot be forbidden outright, because a policy like
    /// collapsing an optional-and-nullable property applies to both halves equally and is not a
    /// finding about either. Repairs are excluded: rewriting is exactly what the 3.0 half is
    /// supposed to need and the 3.1 half is not.
    pub asymmetric: Vec<Difference>,
    /// Everything progeny said about either document.
    pub diagnostics: Vec<Diagnostic>,
}

impl Convergence {
    /// Whether the two documents produced the same model, the same source, and the same losses.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty() && self.output.is_empty() && self.asymmetric.is_empty()
    }
}

/// Check that a 3.0 document and its 3.1 equivalent generate the same thing.
///
/// The two dialects are supposed to converge on one lowering, and a normalization row that is
/// merely *self-consistent* would satisfy the round-trip property while still meaning the wrong
/// thing. This is the check that the rewriting agrees with an independent statement of the same
/// API — which is why the 3.1 half of each pair is hand-written rather than generated.
///
/// Both representations are compared, and the second is the one that matters: the model is an
/// intermediate nobody receives, and convergence is a promise about what is *generated* from the
/// two dialects. Comparing the models as well is worth its cost only because it localizes a
/// failure — a difference visible in the model is a normalization defect, and one visible only in
/// the source is a defect in a later stage.
///
/// The `openapi` member is excluded from the model comparison: it is the one member the two
/// documents are *supposed* to disagree about, and normalization deliberately does not rewrite it.
///
/// # Errors
///
/// Returns [`RejectError`] when either document is unusable.
pub fn convergence(three_zero: &[u8], three_one: &[u8]) -> Result<Convergence, RejectError> {
    let old = generated(three_zero)?;
    let new = generated(three_one)?;

    let mut differences = Vec::new();
    diff(
        &new.model,
        &old.model,
        &JsonPointer::root(),
        &mut differences,
    );
    let mut output = Vec::new();
    diff_files(&new.files, &old.files, &mut output);
    Ok(Convergence {
        differences,
        output,
        asymmetric: asymmetric(&new.diagnostics, &old.diagnostics),
        diagnostics: old.diagnostics.into_iter().chain(new.diagnostics).collect(),
    })
}

/// Run one half of a pair, keeping both representations and its own diagnostics.
///
/// A context per half rather than one shared: what each dialect gave up is only comparable if it
/// is attributable, and a merged list cannot say which document a `Degrade` came from.
fn generated(input: &[u8]) -> Result<Generated, RejectError> {
    let mut ctx = Ctx::new();
    let config = crate::Config::default();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let mut model = doc::serialize::document(&parsed);
    if let Value::Object(root) = &mut model {
        root.remove("openapi");
    }
    // Resolution consumes the parsed document, so the model has to be taken first — the same
    // ordering the round trip needs, and for the same reason.
    let resolved = resolve::resolve(parsed, &mut ctx);
    let shapes = shape::classify(&resolved, &mut ctx);
    let contracts = contract::build(&resolved, &shapes, &config, &mut ctx)?;
    let api = api::build(&resolved, &shapes, &contracts, &config, &mut ctx)?;
    Ok(Generated {
        model,
        files: render::run(&contracts, &api, &config),
        diagnostics: ctx.into_diagnostics(),
    })
}

/// Both representations of one document, so each half of a pair is produced in a single pass.
struct Generated {
    model: Value,
    files: BTreeMap<Utf8PathBuf, String>,
    diagnostics: Vec<Diagnostic>,
}

/// What one dialect gave up and the other did not.
fn asymmetric(new: &[Diagnostic], old: &[Diagnostic]) -> Vec<Difference> {
    let losses = |found: &[Diagnostic]| -> BTreeSet<String> {
        found
            .iter()
            .filter(|found| found.action() != crate::Action::Repair)
            .map(ToString::to_string)
            .collect()
    };
    let (new, old) = (losses(new), losses(old));
    let mut out = Vec::new();
    for detail in new.difference(&old) {
        out.push(Difference {
            location: "3.1".to_owned(),
            detail: detail.clone(),
        });
    }
    for detail in old.difference(&new) {
        out.push(Difference {
            location: "3.0".to_owned(),
            detail: detail.clone(),
        });
    }
    out
}

/// Compare two rendered crates, file by file and then line by line.
///
/// Reported by line rather than as "these files differ" because the useful thing about a
/// convergence failure is *which* declaration moved, and a whole-file inequality says only that
/// something did.
fn diff_files(
    expected: &BTreeMap<Utf8PathBuf, String>,
    actual: &BTreeMap<Utf8PathBuf, String>,
    out: &mut Vec<Difference>,
) {
    const LIMIT: usize = 20;
    for (path, text) in expected {
        if out.len() >= LIMIT {
            return;
        }
        let Some(other) = actual.get(path) else {
            out.push(Difference {
                location: path.to_string(),
                detail: "only one dialect generated this file".to_owned(),
            });
            continue;
        };
        for (line, (want, got)) in text.lines().zip(other.lines()).enumerate() {
            if want == got {
                continue;
            }
            out.push(Difference {
                location: format!("{path}:{}", line + 1),
                detail: format!(
                    "3.1 renders `{}`, 3.0 renders `{}`",
                    want.trim(),
                    got.trim()
                ),
            });
            break;
        }
        let (want, got) = (text.lines().count(), other.lines().count());
        if want != got {
            out.push(Difference {
                location: path.to_string(),
                detail: format!("3.1 renders {want} lines, 3.0 renders {got}"),
            });
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            out.push(Difference {
                location: path.to_string(),
                detail: "only one dialect generated this file".to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_two_dialects_converge_on_one_model() {
        // The committed pair, so the check runs offline in `task test` as well as in the corpus.
        const OLD: &[u8] = include_bytes!("../../../../corpus/convergence/dialects.3.0.yaml");
        const NEW: &[u8] = include_bytes!("../../../../corpus/convergence/dialects.3.1.yaml");
        let result = super::convergence(OLD, NEW).unwrap();
        // Three assertions rather than one `is_clean`, because they fail for three different
        // reasons: a model difference is a normalization defect, a source difference with the
        // models agreeing is a defect in a stage after it, and an asymmetric loss means one dialect
        // was understood less well than the other.
        assert!(result.differences.is_empty(), "{:#?}", result.differences);
        assert!(result.output.is_empty(), "{:#?}", result.output);
        assert!(result.asymmetric.is_empty(), "{:#?}", result.asymmetric);
        assert!(result.is_clean());
        // Two empty renderings compare equal, so the source half of this gate would pass whether or
        // not it worked. What makes it a check is that there was something to compare.
        let rendered = super::generated(NEW).unwrap().files;
        let types = &rendered[camino::Utf8Path::new("src/types.rs")];
        assert!(types.contains("pub struct"), "{types}");
        assert!(types.contains("pub enum"), "{types}");
    }

    #[test]
    fn a_source_difference_is_reported_by_file_and_line() {
        let file = |name: &str, text: &str| {
            let mut map = std::collections::BTreeMap::new();
            map.insert(camino::Utf8PathBuf::from(name), text.to_owned());
            map
        };

        let mut found = Vec::new();
        super::diff_files(
            &file(
                "src/types.rs",
                "pub struct Pet {\n    pub name: String,\n}\n",
            ),
            &file("src/types.rs", "pub struct Pet {\n    pub name: i64,\n}\n"),
            &mut found,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].location, "src/types.rs:2");

        // A file only one dialect produced is a difference even though no line disagrees.
        let mut found = Vec::new();
        super::diff_files(
            &file("src/types.rs", "a\n"),
            &file("src/other.rs", "a\n"),
            &mut found,
        );
        assert_eq!(found.len(), 2);

        // Equal as far as the shorter one goes, and still not the same crate.
        let mut found = Vec::new();
        super::diff_files(
            &file("src/a.rs", "a\nb\n"),
            &file("src/a.rs", "a\n"),
            &mut found,
        );
        assert_eq!(found.len(), 1);
        assert!(found[0].detail.contains("2 lines"), "{found:#?}");
    }

    #[test]
    fn a_loss_only_one_dialect_suffered_is_reported_and_a_shared_one_is_not() {
        use crate::{Action, BreakageClass, Diagnostic, JsonPointer};

        let record = |detail: &str| {
            vec![Diagnostic::new(
                BreakageClass::WildUnion,
                Action::Degrade,
                JsonPointer::root(),
                detail,
            )]
        };
        let shared = record("nothing tells these apart");
        let other = record("something else entirely");

        assert!(super::asymmetric(&shared, &shared).is_empty());
        let found = super::asymmetric(&shared, &other);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].location, "3.1");
        assert_eq!(found[1].location, "3.0");
    }
}
