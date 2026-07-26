//! Generate a Rust client and server from an OpenAPI description.
//!
//! progeny reads an OpenAPI 3.0.x or 3.1.x document — JSON or YAML, including the imperfect
//! documents large real services actually publish — and produces the calling side, the serving
//! side, and the shared type layer, so that the program and the description cannot disagree.
//!
//! Three properties govern the whole design:
//!
//! * **Parse, don't validate.** Untrusted input becomes a typed value once, at the edge.
//!   Downstream code cannot receive malformed data because there is no shape for it to arrive in.
//! * **One direction.** `load → normalize → document → shape → contract → api → render`. Each
//!   conversion is total: it produces a value plus diagnostics, or it rejects.
//! * **Silently wrong output is the only forbidden failure mode.** Generating less, with a
//!   diagnostic, always beats generating something plausible. Every deviation from the input
//!   document appears in [`Output::diagnostics`]; the caller decides which ones stop the build.
//!
//! # Status
//!
//! The front end is implemented: loading, dialect normalization, and the lossless document and
//! schema model, with the model's fidelity gated by a round trip over a corpus of 78 published
//! descriptions. No renderer exists yet, so [`generate`] returns diagnostics and no files.

mod config;
mod diag;
mod doc;
mod load;
mod normalize;
mod schema;
mod value;

#[cfg(feature = "harness")]
#[doc(hidden)]
pub mod harness;

use std::collections::BTreeMap;

use camino::Utf8PathBuf;

pub use crate::config::{Config, Deny};
pub use crate::diag::{Action, BreakageClass, Diagnostic, JsonPointer, RejectError, RejectKind};

/// Generated source, and everything progeny had to say about the document.
///
/// `diagnostics` is not optional and not a side channel: the only way to obtain generated source
/// is to obtain the diagnostics with it, which is what makes "silently degraded" unrepresentable
/// at the boundary.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// Rendered source, keyed by path relative to the output root.
    pub files: BTreeMap<Utf8PathBuf, String>,
    /// Every deviation from the input document, in the order they were found.
    pub diagnostics: Vec<Diagnostic>,
}

/// Read a document and generate from it.
///
/// Takes bytes and returns strings: the library performs no I/O, which keeps generation
/// deterministic and trivially testable, and leaves the filesystem to the caller.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable — unparsable bytes, a version progeny
/// does not implement, no operations at all. Rejection is a last resort and it is total: there is
/// no partial rejection, so anything short of it produces output plus diagnostics.
pub fn generate(input: &[u8], config: &Config) -> Result<Output, RejectError> {
    // Nothing in `Config` reaches the front end: the customization set is consumed where
    // contracts are built, and strictness is the caller's policy over the returned diagnostics.
    // It is in the signature already so that the boundary does not change shape later.
    let _ = config;

    let mut ctx = diag::Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);

    // The renderers do not exist yet; the model does, and it is what the corpus gate measures.
    let _ = parsed;

    Ok(Output {
        files: BTreeMap::new(),
        diagnostics: ctx.into_diagnostics(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Config, RejectKind, generate};

    const PETSTORE: &[u8] = include_bytes!("../../../corpus/specs/petstore-31.yaml");

    #[test]
    fn a_document_that_needs_no_repair_produces_no_diagnostics() {
        let output = generate(PETSTORE, &Config::default()).unwrap();
        assert!(
            output.diagnostics.is_empty(),
            "{:?}",
            output
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn there_is_nothing_to_render_yet() {
        let output = generate(PETSTORE, &Config::default()).unwrap();
        assert!(output.files.is_empty());
    }

    #[test]
    fn unusable_documents_are_rejected_rather_than_half_generated() {
        for (input, kind) in [
            (&b"not a document at all: ["[..], RejectKind::Unparsable),
            (&b"[]"[..], RejectKind::NotAnObject),
            (&b"{}"[..], RejectKind::MissingVersion),
            (
                &b"{\"openapi\": \"2.0\", \"paths\": {}}"[..],
                RejectKind::UnsupportedVersion,
            ),
            (&b"{\"openapi\": \"3.1.0\"}"[..], RejectKind::NoOperations),
        ] {
            let error = generate(input, &Config::default()).unwrap_err();
            assert_eq!(error.kind(), kind, "{}", String::from_utf8_lossy(input));
        }
    }
}
