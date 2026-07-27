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
//! resolution, shape classification, the wire contracts, and all three renderers — the shared type
//! layer, the client, and the server. The model's fidelity is gated by a round trip over a corpus
//! of 78 published descriptions; the generated crates are gated by compiling *and linting* them,
//! by running serde against the payloads those descriptions carry, and by standing one generated
//! client up against the generated server of the same document over a real socket.
//!
//! # Serde strategy, and the one case where it matters
//!
//! Types are rendered with **hand-written** `Serialize`/`Deserialize` implementations by default,
//! wherever [`SerdeImpl`]'s eligibility rules allow. This is a compile-time decision and not a
//! behavioural one: the two strategies are asserted equivalent on the wire by a differential
//! harness, and they agree on every payload in the corpus. The saving is worth having — on the type
//! layer, 65–67% less CPU and 47–55% less peak RSS than the derive.
//!
//! It costs one thing. The hand-written path for **structs** buffers members before assigning them,
//! which requires a *self-describing* format — one that names its members, as JSON, YAML and TOML
//! do. Feeding generated structs to a format that does not, such as `bincode` or `postcard`, needs
//! [`SerdeImpl::DeriveAlways`], which puts every type back on the derive. Fieldless enums are
//! unaffected either way: that path resolves a variant from its identifier and never buffers.
//!
//! # Pagination
//!
//! Declared per operation with [`Pagination`], and **never detected**. 62 of the 78 corpus
//! documents paginate and no two agree on how to say so — the cursor parameter is called `offset`
//! 541 times, `page` 319, `cursor` 213, `after` 198 — so detection would be a table of vendor
//! spellings pretending to be a rule. A declaration names the cursor query parameter, the member
//! holding the next cursor and the member holding the items, and every one of them is checked
//! against the document before anything is generated: a name that does not resolve is an error
//! that says what it looked for and what the document had instead.
//!
//! A declared operation gains a `stream()` beside its `send()` — never instead of it. The generated
//! crate depends on `futures-core` and `futures-util` only when some operation declared pagination.

mod api;
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
    BodyLimit, BytesRepr, Config, DateTimeCrate, Deny, Derive, Emit, Formats, MapKind, Package,
    Packaging, Pagination, SerdeImpl, UnknownFields, UuidCrate,
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
    // Built whether or not a client will be rendered. Its diagnostics are findings about the
    // document — two operations that collide, an example that contradicts its own schema, which
    // half of the API a presence collapse costs — and a caller who asked for types only is still
    // entitled to them.
    let api = api::build(&resolved, &shapes, &contracts, config, &mut ctx)?;

    Ok(Output {
        files: render::run(&contracts, &api, config),
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
    fn the_shared_type_layer_and_the_client_are_generated() {
        let output = generate(PETSTORE, &Config::default()).unwrap();
        let names: Vec<&str> = output.files.keys().map(|path| path.as_str()).collect();
        assert_eq!(
            names,
            [
                "Cargo.toml",
                "src/client.rs",
                "src/lib.rs",
                "src/server.rs",
                "src/support.rs",
                "src/types.rs"
            ]
        );
        let types = &output.files[camino::Utf8Path::new("src/types.rs")];
        assert!(types.contains("pub struct Pet"), "{types}");
        let client = &output.files[camino::Utf8Path::new("src/client.rs")];
        assert!(client.contains("pub struct Client"), "{client}");
        assert!(client.contains("pub fn list_pets"), "{client}");
        let server = &output.files[camino::Utf8Path::new("src/server.rs")];
        assert!(server.contains("pub trait Api"), "{server}");
        assert!(server.contains("fn list_pets"), "{server}");
        assert!(server.contains("pub fn router"), "{server}");
        // Every rendered file has to parse as Rust, or the compile gate is the first thing to
        // find out.
        for (path, text) in &output.files {
            if path.extension() == Some("rs") {
                syn::parse_file(text)
                    .unwrap_or_else(|error| panic!("{path} does not parse: {error}"));
            }
        }
    }

    #[test]
    fn a_description_with_no_operations_generates_no_client() {
        let types_only = generate(
            br#"{"openapi":"3.1.0","paths":{},"components":{"schemas":{"Pet":{"type":"object"}}}}"#,
            &Config::default(),
        )
        .unwrap();
        assert!(
            !types_only
                .files
                .contains_key(camino::Utf8Path::new("src/client.rs"))
        );

        // The case that makes this a decision rather than an accident: the document *has* an
        // operation, and it is unfillable, so nothing is left to call. A client with no methods —
        // and the whole HTTP dependency behind it — would still be emitted if emission were keyed
        // on the configuration rather than on there being something to call.
        let output = generate(
            br#"{"openapi":"3.1.0","paths":{"/pets/{id}":{"get":{"responses":{"200":{"description":"ok"}}}}}}"#,
            &Config::default(),
        )
        .unwrap();
        let names: Vec<&str> = output.files.keys().map(|path| path.as_str()).collect();
        assert_eq!(names, ["Cargo.toml", "src/lib.rs", "src/types.rs"]);
        assert!(!output.files[camino::Utf8Path::new("Cargo.toml")].contains("reqwest"));
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
