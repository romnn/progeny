//! The payload gate's input: every example a document offers, with the expected round trip.

use proc_macro2::TokenStream;
use serde_json::Value;

use crate::diag::{Ctx, RejectError};
use crate::render::types::{Scope, format_type};
use crate::shape::Format;
use crate::{api, contract, doc, load, normalize, resolve, shape};

/// One example payload, and what the generated types must do with it.
#[derive(Debug, Clone)]
pub struct Payload {
    /// Where the example was written, as a JSON Pointer into the document.
    pub location: String,
    /// The generated type the payload deserializes into, by its name in the `types` module or,
    /// with [`Payload::read`], in `types::read`.
    pub type_name: String,
    /// Whether the type lives in `types::read`, where a response's read forms are.
    pub read: bool,
    /// The payload exactly as the document wrote it.
    pub original: Value,
    /// The payload restricted to what the document declares.
    ///
    /// What serializing the deserialized value back must produce. Compared against *this* rather
    /// than against a second round of the type's own output, because a member the type drops
    /// uniformly survives an idempotence check forever.
    pub expected: Value,
    /// Whether the document's own example contradicts its own schema, so a failure here is a
    /// finding about the vendor rather than about progeny.
    pub vendor_defect: bool,
    /// The strings [`Payload::expected`] reached through a format.
    ///
    /// The generated type may write one in a spelling of its own, such as an IPv6 address
    /// compressed, and a round trip keeps it when that type reads both spellings as one value.
    pub formatted: Vec<Formatted>,
}

/// A string a payload holds in a formatted type.
#[derive(Debug, Clone)]
pub struct Formatted {
    /// Where the string sits, as a JSON Pointer into the payload.
    pub at: String,
    /// The type the generated code holds it in, spelled from a crate that names the generated
    /// crate `generated`.
    pub ty: TokenStream,
    /// Whether it is a date-time or a time, whose type keeps no digit past the nanosecond and may
    /// not keep a leap second.
    pub clock: bool,
}

/// The payloads a document offers, and what was left out.
#[derive(Debug, Clone, Default)]
pub struct Payloads {
    pub payloads: Vec<Payload>,
    /// Examples at a position whose type is arbitrary JSON: a round trip through it asserts
    /// nothing.
    pub opaque: usize,
    /// Examples at a position whose type is spelled at the use site rather than named, so a
    /// generated test cannot name it either.
    pub unnamed: usize,
    /// Examples at a position whose type keeps members the schema does not declare, where "what a
    /// faithful round trip keeps" is not the pruned payload.
    pub captures: usize,
}

/// Collect every example payload the API surface carries.
///
/// The input to the payload gate: the first check in the project that runs serde against real
/// data rather than asking whether source compiles.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn payloads(input: &[u8], config: &crate::Config) -> Result<Payloads, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let resolved = resolve::resolve(parsed, &mut ctx);
    let shapes = shape::classify(&resolved, config.formats.integers, &mut ctx);
    let contracts = contract::build(&resolved, &shapes, config, &mut ctx)?;
    let model = api::build(&resolved, &shapes, &contracts, config, &mut ctx)?;

    let (found, skipped) = api::payloads(&resolved, &shapes, &contracts, &model);
    let count = |wanted: api::Skipped| skipped.iter().filter(|it| **it == wanted).count();
    Ok(Payloads {
        opaque: count(api::Skipped::Opaque),
        unnamed: count(api::Skipped::Unnamed),
        captures: count(api::Skipped::Captures),
        payloads: found
            .into_iter()
            .map(|payload| Payload {
                location: payload.location,
                type_name: payload.type_name,
                read: payload.read,
                original: payload.original,
                expected: payload.expected,
                vendor_defect: payload.vendor_defect,
                formatted: payload
                    .formatted
                    .into_iter()
                    .map(|leaf| Formatted {
                        at: leaf.at,
                        ty: format_type(leaf.format, config, Scope::External),
                        clock: matches!(leaf.format, Format::DateTime | Format::Time),
                    })
                    .collect(),
            })
            .collect(),
    })
}
