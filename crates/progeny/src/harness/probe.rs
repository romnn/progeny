//! The wire-differential plan: every operation of a document, ready to be sent by its own client.
//!
//! The example gate proved that the only assertions which catch request-line defects are the ones
//! that *send a request* — and it covered one document. This closes the gap for the rest: from the
//! same frozen contracts the renderers read, it synthesizes a value for every parameter, body and
//! response of every servable operation, so `xtask probe` can generate the recorder double and the
//! driver tests for any corpus document instead of hand-writing them for petstore.
//!
//! The plan deliberately does not assert value equality. The two defects the example crate caught —
//! path parameters read from the wrong place, no coercion of the text a URL carries — both surfaced
//! as *rejections of a well-formed request*. So the probe's claim is exactly that: a request built
//! by the generated client, with every declared parameter set, extracts cleanly in the generated
//! server, reaches the handler with every optional parameter present, and the declared response
//! decodes back in the client. Anything an operation needs that cannot be synthesized — a recursive
//! type with no optional escape, a success the document never declares — is a *named skip*, counted
//! out loud, never a silent cap.

use crate::api::{
    BodyContract, Location, OperationContract, ResponseArm, ResponseBody, StatusPattern,
};
use crate::config::Config;
use crate::contract::{ContractKind, Contracts, TypeIndex, TypeRef};
use crate::diag::{Ctx, RejectError};
use crate::shape::Format;
use crate::{api, contract, doc, load, normalize, render, resolve, shape};

/// Everything `xtask probe` needs to write the double and the driver for one document.
#[derive(Debug, Clone)]
pub struct Probe {
    /// One entry per operation the router accepted, probed or skipped.
    pub operations: Vec<ProbeOp>,
}

/// One operation: the trait method the double implements, and the request the driver sends.
#[derive(Debug, Clone)]
pub struct ProbeOp {
    /// The generated client method, which is also the trait method's name.
    pub method: String,
    /// Whether exercising the operation deliberately calls deprecated API.
    pub deprecated: bool,
    /// The grouped parameter arguments, in trait-signature order.
    pub groups: Vec<ProbeGroup>,
    /// The body argument, when the operation takes one.
    pub body: Option<ProbeBody>,
    /// One call per parameter, then `.body(...)` when there is one.
    pub setters: Vec<ProbeSetter>,
    /// What the double answers, and what the driver expects back.
    pub response: ProbeResponse,
    /// Why this operation is implemented as `unimplemented!()` and never driven, when it is.
    pub skip: Option<String>,
}

/// One location's group struct in the trait signature.
#[derive(Debug, Clone)]
pub struct ProbeGroup {
    /// The argument name: `path`, `query`, `header` or `cookie`.
    pub arg: &'static str,
    /// The struct's name inside the generated `server` module.
    pub ty: String,
    /// The fields the document made optional, which the driver always sets — so the double
    /// asserts each arrived `Some`, which is what catches a parameter dropped in transit.
    pub optional_fields: Vec<ProbeField>,
}

/// One optional parameter field whose presence the generated double checks.
#[derive(Debug, Clone)]
pub struct ProbeField {
    /// The field's Rust name.
    pub name: String,
    /// Whether reading the field deliberately exercises deprecated API.
    pub deprecated: bool,
}

/// The request body the driver sets.
#[derive(Debug, Clone)]
pub struct ProbeBody {
    /// The Rust path of the body type, spelled from inside the generated crate.
    pub ty: String,
    /// Whether naming the body type deliberately exercises deprecated API.
    pub deprecated: bool,
    /// A JSON value of that type, synthesized from its contract.
    pub json: String,
    /// Whether the document marked it required. An optional body reaches the trait as an
    /// `Option`, so the double's signature and its presence assertion both turn on this.
    pub required: bool,
}

/// One builder call the driver makes.
#[derive(Debug, Clone)]
pub struct ProbeSetter {
    /// The setter's name, which is the parameter's Rust name.
    pub setter: String,
    /// Whether calling the setter deliberately exercises deprecated API.
    pub deprecated: bool,
    /// The Rust path of the parameter's type.
    pub ty: String,
    /// A JSON value of that type.
    pub json: String,
}

/// The response the double constructs, and the status the driver asserts came back.
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    /// The response enum's name inside the generated `server` module.
    ///
    /// Needed whether or not the operation is driven: the double has to *implement* a skipped
    /// operation's trait method either way, and the method's return type names this.
    pub enum_name: String,
    /// The arm the double answers with — absent for an operation the driver never calls.
    ///
    /// An `Option` rather than empty-string sentinels: a consumer that read a sentinel's
    /// `status: 0` before checking the skip would emit an assertion on a status nothing sends,
    /// and the type now makes that read impossible.
    pub answer: Option<ProbeAnswer>,
}

/// The synthesized reply for one driven operation.
#[derive(Debug, Clone)]
pub struct ProbeAnswer {
    /// The variant for the arm the double answers with.
    pub variant: String,
    /// Whether the response enum indirects this non-unit payload.
    pub boxed: bool,
    /// The payload in the native representation its response arm carries.
    pub value: ProbeValue,
    /// The status the client must see.
    pub status: u16,
}

/// A synthesized response payload, distinguished by its wire representation.
#[derive(Debug, Clone)]
pub enum ProbeValue {
    /// A schema-derived value serialized as JSON.
    Json {
        /// A JSON value of that type.
        json: String,
    },
    /// Text written verbatim.
    Text(String),
    /// Bytes written without JSON serialization.
    Bytes {
        /// The configured Rust byte representation.
        ty: String,
        /// The raw octets.
        bytes: Vec<u8>,
    },
    /// No response body.
    Empty,
}

/// Build the probe plan for one document.
///
/// # Errors
///
/// Returns [`RejectError`] when the document itself is unusable, exactly as [`crate::generate`]
/// would; a merely unprobeable *operation* is a named skip in the plan instead.
pub fn probe(input: &[u8], config: &Config) -> Result<Probe, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let resolved = resolve::resolve(parsed, &mut ctx);
    let shapes = shape::classify(&resolved, &mut ctx);
    let contracts = contract::build(&resolved, &shapes, config, &mut ctx)?;
    let model = api::build(&resolved, &shapes, &contracts, config, &mut ctx)?;

    let operations = model
        .operations()
        .iter()
        .filter(|operation| operation.registrable.is_some())
        .map(|operation| plan(operation, &contracts, config))
        .collect();
    Ok(Probe { operations })
}

/// The plan for one servable operation, with the reason when it cannot be driven.
fn plan(operation: &OperationContract, contracts: &Contracts, config: &Config) -> ProbeOp {
    let groups = render::server::LOCATIONS
        .iter()
        .filter_map(|&(location, suffix)| {
            let mut any = false;
            let optional_fields: Vec<ProbeField> = operation
                .params_at(location)
                .inspect(|_| any = true)
                .filter(|param| !param.required)
                // An exploded `form` object cannot be read back by name — the encoding erases the
                // parameter's name from the wire — so the probe neither sets nor asserts it. Not a
                // narrowing of the operation: everything recoverable is still exercised.
                .filter(|param| !param.style.erased_by_explosion())
                .map(|param| ProbeField {
                    name: param.rust_name.as_str().to_owned(),
                    deprecated: param.docs.deprecated,
                })
                .collect();
            any.then(|| ProbeGroup {
                arg: argument_of(location),
                ty: render::server::group_name(operation, suffix).to_string(),
                optional_fields,
            })
        })
        .collect();

    let body = operation.body.as_ref().map(|body| {
        let (ty, deprecated) = test_type(&render::client::body_type(body, contracts, config));
        ProbeBody {
            ty,
            deprecated,
            json: String::new(),
            required: body.required(),
        }
    });

    // Skips are decided after the signature is known, because the double has to *implement* a
    // skipped operation's trait method either way — only the driver test is dropped.
    let mut skip = None;

    let setters = probe_setters(operation, contracts, config, &mut skip);

    let body = body.map(|mut planned| {
        let ty = match operation.body.as_ref() {
            Some(
                BodyContract::Json { ty, .. }
                | BodyContract::Form { ty, .. }
                | BodyContract::Multipart { ty, .. },
            ) => Some(ty),
            Some(BodyContract::Text { .. } | BodyContract::Bytes { .. }) | None => None,
        };
        match ty.map(|ty| synthesize(ty, contracts, &mut Vec::new())) {
            Some(Ok(value)) => planned.json = value.to_string(),
            Some(Err(reason)) => {
                skip.get_or_insert(format!("the body cannot be synthesized: {reason}"));
            }
            // `Text`: the body is a `String`, and a JSON string deserializes into it. `Bytes` is
            // `Vec<u8>`, which serde reads from a sequence, so the probe spells its bytes out.
            None => {
                planned.json = if matches!(operation.body, Some(BodyContract::Bytes { .. })) {
                    "[112,114,111,98,101]".to_owned()
                } else {
                    "\"probe\"".to_owned()
                };
            }
        }
        planned
    });

    let answer = match probe_answer(operation, contracts, config) {
        Ok(answer) => Some(answer),
        Err(reason) => {
            skip.get_or_insert(reason);
            None
        }
    };
    let response = ProbeResponse {
        enum_name: render::server::response_name(operation).to_string(),
        answer,
    };

    ProbeOp {
        method: operation.rust_name.as_str().to_owned(),
        deprecated: operation.docs.deprecated,
        groups,
        body,
        setters,
        response,
        skip,
    }
}

fn probe_answer(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> Result<ProbeAnswer, String> {
    let (arm, status) = success_arm(operation).ok_or_else(|| {
        "no declared success status; only `default` or errors, and which status a `default` answers \
         with is the handler's choice rather than the description's"
            .to_owned()
    })?;
    let value = match &arm.body {
        ResponseBody::Json(ty) => ProbeValue::Json {
            json: synthesize(ty, contracts, &mut Vec::new())
                .map_err(|reason| format!("the response cannot be synthesized: {reason}"))?
                .to_string(),
        },
        ResponseBody::Text { .. } => ProbeValue::Text("probe".to_owned()),
        ResponseBody::Bytes { .. } => ProbeValue::Bytes {
            ty: test_path(&render::types::response_type_path(
                &arm.body, contracts, config,
            )),
            bytes: b"probe".to_vec(),
        },
        ResponseBody::Empty => ProbeValue::Empty,
    };
    Ok(ProbeAnswer {
        variant: arm.rust_name.as_str().to_owned(),
        boxed: crate::render::types::response_body_is_boxed(&arm.body),
        value,
        status,
    })
}

/// A rendered type path, as the probe's test file spells it.
///
/// The renderer writes `super::types::X` because `client` and `server` are sibling modules of
/// `types`; the test file imports `types` at its own root (`use {krate}::{client, server,
/// types};`), so the same type is `types::X` there. Respelled here, beside the renderer that
/// owns the original spelling — the consumer once patched it back with its own string
/// replacement, and a changed qualification would have slipped past that silently.
fn test_path(rendered: &proc_macro2::TokenStream) -> String {
    test_type(rendered).0
}

fn test_type(rendered: &proc_macro2::TokenStream) -> (String, bool) {
    let rendered = rendered.to_string();
    let deprecated = rendered.contains("super :: types :: __progeny_deprecated :: ");
    let path = rendered
        .replace("super :: types :: __progeny_deprecated :: ", "types::")
        .replace("super :: types :: ", "types::")
        .replace(' ', "");
    (path, deprecated)
}

/// One synthesized setter per settable parameter, recording a skip where none can be.
fn probe_setters(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
    skip: &mut Option<String>,
) -> Vec<ProbeSetter> {
    let mut setters = Vec::new();
    for param in &operation.params {
        if param.style.erased_by_explosion() {
            // The wire erases the parameter's name, so the server can never see it. Optional:
            // the client simply leaves it unset and the operation is still probeable. Required:
            // the builder refuses to send without it and the handler rejects without it, so the
            // operation cannot cross the wire at all — a named skip, not a red test and not a
            // silent cap.
            if param.required {
                skip.get_or_insert(format!(
                    "parameter `{}` is a required exploded `form` object, whose name the wire \
                     erases; no request can carry it in a form the server can read",
                    param.wire_name
                ));
            }
            continue;
        }
        // The raw text and byte fixtures are both the five bytes `probe`. A different synthesized
        // `Content-Length` makes the HTTP server wait forever for bytes the client will never send,
        // which the Cloudflare R2 `PUT object` operation exposed.
        let value = if param.style.location() == Location::Header
            && param.wire_name.eq_ignore_ascii_case("content-length")
            && matches!(
                operation.body,
                Some(BodyContract::Text { .. } | BodyContract::Bytes { .. })
            ) {
            Ok(serde_json::json!(5))
        } else {
            synthesize(&param.ty, contracts, &mut Vec::new())
        };
        match value {
            Ok(value) => {
                let (ty, deprecated_type) =
                    test_type(&render::types::type_path(&param.ty, contracts, config));
                setters.push(ProbeSetter {
                    setter: param.rust_name.as_str().to_owned(),
                    deprecated: param.docs.deprecated || deprecated_type,
                    ty,
                    json: value.to_string(),
                });
            }
            Err(reason) => {
                skip.get_or_insert(format!(
                    "parameter `{}` cannot be synthesized: {reason}",
                    param.wire_name
                ));
            }
        }
    }
    setters
}

/// The first arm the driver can wait for: an exact 2xx, or a 2XX range.
///
/// Not `default` — that variant carries its own status precisely because the description does not
/// say which one it means, and a probe asserting a number the description never stated would be
/// testing the double rather than the wire.
fn success_arm(operation: &OperationContract) -> Option<(&ResponseArm, u16)> {
    let exact = operation
        .responses
        .arms
        .iter()
        .find_map(|arm| match arm.status {
            StatusPattern::Exact(code) if (200..300).contains(&code) => Some((arm, code)),
            _ => None,
        });
    exact.or_else(|| {
        operation
            .responses
            .arms
            .iter()
            .find_map(|arm| matches!(arm.status, StatusPattern::Range(2)).then_some((arm, 200)))
    })
}

fn argument_of(location: Location) -> &'static str {
    match location {
        Location::Path => "path",
        Location::Query => "query",
        Location::Header => "header",
        Location::Cookie => "cookie",
    }
}

/// A JSON value of the given type, built from the same contracts the deserializer was.
///
/// Every choice here is the least surprising member of the type: the first declared variant, one
/// element, every member present — optional ones included, because a probe that omits what it may
/// omit is not exercising the reader. The one legitimate failure is recursion with no optional
/// escape: a value of such a type has no finite spelling, and the operation is skipped by name.
fn synthesize(
    ty: &TypeRef,
    contracts: &Contracts,
    visiting: &mut Vec<TypeIndex>,
) -> Result<serde_json::Value, SynthesisError> {
    use serde_json::{Value, json};
    Ok(match ty {
        TypeRef::Unit => Value::Null,
        TypeRef::Bool => json!(true),
        TypeRef::I64 | TypeRef::U64 => json!(7),
        TypeRef::F64 => json!(0.5),
        TypeRef::String => json!("a"),
        TypeRef::Value => json!({"probe": true}),
        TypeRef::Format(format) => match format {
            // The dependency-free defaults render every format as text, but the *content* still has
            // to be the format's, so a typed configuration parses the same probe.
            Format::DateTime => json!("2020-01-01T00:00:00Z"),
            Format::Date => json!("2020-01-01"),
            Format::Time => json!("12:00:00"),
            Format::Uuid => json!("00000000-0000-0000-0000-000000000000"),
            Format::Ip | Format::Ipv4 => json!("192.0.2.1"),
            Format::Ipv6 => json!("2001:db8::1"),
            Format::Base64 => json!("cHJvYmU="),
            Format::Binary => json!("probe"),
        },
        TypeRef::Option(inner) | TypeRef::Boxed(inner) => synthesize(inner, contracts, visiting)?,
        TypeRef::Vec(inner) => json!([synthesize(inner, contracts, visiting)?]),
        // One member, not none: an empty map writes nothing on the wire, and a probe that sends
        // nothing asserts nothing — `jellyfin`'s `deepObject` maps only round-trip because this
        // carries a byte.
        TypeRef::Map(inner) => json!({"probe": synthesize(inner, contracts, visiting)?}),
        TypeRef::Array(inner, len) => {
            let element = synthesize(inner, contracts, visiting)?;
            Value::Array(std::iter::repeat_n(element, *len as usize).collect())
        }
        TypeRef::Tuple(items) => Value::Array(
            items
                .iter()
                .map(|item| synthesize(item, contracts, visiting))
                .collect::<Result<_, _>>()?,
        ),
        TypeRef::Named(index) => {
            let Some(contract) = contracts.get(*index) else {
                return Err(SynthesisError::UnresolvableType);
            };
            if visiting.contains(index) {
                return Err(SynthesisError::Recursive {
                    name: contract.rust_name().as_str().to_owned(),
                });
            }
            // Pushed here and popped here, whatever `synthesize_named` returns — keeping the
            // cleanup in one frame is what lets that function use `?` freely.
            visiting.push(*index);
            let value = synthesize_named(contract, contracts, visiting);
            visiting.pop();
            value?
        }
    })
}

/// A value of one named contract. The caller owns the recursion guard.
fn synthesize_named(
    contract: &crate::contract::TypeContract,
    contracts: &Contracts,
    visiting: &mut Vec<TypeIndex>,
) -> Result<serde_json::Value, SynthesisError> {
    use serde_json::{Map, Value, json};
    Ok(match contract.kind() {
        ContractKind::Struct { fields } => {
            let mut members = Map::new();
            for field in fields {
                // The capture member is the landing place for undeclared members, and the probe
                // sends none.
                if field.flatten {
                    continue;
                }
                match synthesize(&field.ty, contracts, visiting) {
                    Ok(value) => {
                        members.insert(field.wire_name.clone(), value);
                    }
                    // An optional member that cannot be synthesized — a recursive type reaching
                    // itself — is legitimately absent; that is what the escape *is*. A required
                    // one fails the whole value.
                    Err(reason) => {
                        if !matches!(field.ty, TypeRef::Option(_)) {
                            return Err(reason);
                        }
                    }
                }
            }
            Value::Object(members)
        }
        ContractKind::StringEnum { variants } => match variants.first() {
            Some(variant) => json!(variant.wire_name),
            None => {
                return Err(SynthesisError::EmptyStringEnum {
                    name: contract.rust_name().as_str().to_owned(),
                });
            }
        },
        ContractKind::Enum { variants } => {
            let Some(variant) = variants.first() else {
                return Err(SynthesisError::EmptyUnion {
                    name: contract.rust_name().as_str().to_owned(),
                });
            };
            synthesize(&variant.ty, contracts, visiting)?
        }
        ContractKind::TaggedEnum { tag, variants } => {
            let Some(variant) = variants.first() else {
                return Err(SynthesisError::EmptyUnion {
                    name: contract.rust_name().as_str().to_owned(),
                });
            };
            let mut value = synthesize(&variant.ty, contracts, visiting)?;
            if let Value::Object(members) = &mut value {
                members.insert(tag.clone(), json!(variant.tag_value));
            }
            value
        }
        ContractKind::Newtype { inner } | ContractKind::Alias { target: inner } => {
            synthesize(inner, contracts, visiting)?
        }
        ContractKind::Tuple { items } => Value::Array(
            items
                .iter()
                .map(|item| synthesize(item, contracts, visiting))
                .collect::<Result<_, _>>()?,
        ),
    })
}

#[derive(Debug, thiserror::Error)]
enum SynthesisError {
    #[error("an unresolvable type index")]
    UnresolvableType,
    #[error("`{name}` is recursive with no optional escape on this path")]
    Recursive { name: String },
    #[error("`{name}` is an enum with no variants")]
    EmptyStringEnum { name: String },
    #[error("`{name}` is a union with no variants")]
    EmptyUnion { name: String },
}
