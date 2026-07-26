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
//! Implemented: loading, dialect normalization, the lossless document and schema model, reference
//! resolution, shape classification, the wire contracts, and the types renderer. The model's
//! fidelity is gated by a round trip over a corpus of 78 published descriptions, and the generated
//! types are gated by compiling them. The client and server renderers do not exist yet, so
//! [`generate`] emits the shared type layer and nothing else.

mod catalogue;
mod config;
mod contract;
mod diag;
mod doc;
mod load;
mod normalize;
mod render;
mod resolve;
mod schema;
mod shape;
mod support;
mod value;

#[cfg(feature = "harness")]
#[doc(hidden)]
pub mod harness;

use std::collections::BTreeMap;

use camino::Utf8PathBuf;

pub use crate::config::{
    BytesRepr, Config, DateTimeCrate, Deny, Derive, Emit, Formats, MapKind, Package, Packaging,
    SerdeImpl, UnknownFields, UuidCrate,
};
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
    let mut ctx = diag::Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let resolved = resolve::resolve(parsed, &mut ctx);
    let shapes = shape::classify(&resolved, &mut ctx);
    let contracts = contract::build(&resolved, &shapes, config, &mut ctx)?;

    Ok(Output {
        files: render::run(&contracts, config),
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
    fn the_shared_type_layer_is_generated() {
        let output = generate(PETSTORE, &Config::default()).unwrap();
        let names: Vec<&str> = output.files.keys().map(|path| path.as_str()).collect();
        assert_eq!(names, ["Cargo.toml", "src/lib.rs", "src/types.rs"]);
        let types = &output.files[camino::Utf8Path::new("src/types.rs")];
        assert!(types.contains("pub struct Pet"), "{types}");
        // Every rendered file has to parse as Rust, or the compile gate is the first thing to
        // find out.
        syn::parse_file(types).unwrap();
    }

    #[test]
    fn a_rejected_configuration_stops_generation_rather_than_producing_half_of_it() {
        let config: Config = toml::from_str("[type-derives]\nPet = [\"copy\"]\n").unwrap();
        let error = generate(PETSTORE, &config).unwrap_err();
        assert_eq!(error.kind(), RejectKind::UnsatisfiableConfig);
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
