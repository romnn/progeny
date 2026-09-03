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
    /// The `HEAD` request the driver sends to a `GET` route, to check that the reflection
    /// resolves what axum dispatched rather than only what the description declared.
    ///
    /// `None` when the document has no driven, parameterless `GET` to send it to, which
    /// `xtask probe` says out loud rather than passing silently.
    pub head: Option<ProbeHead>,
}

/// A `HEAD` request axum serves with a `GET` handler, and the route the reflection must name.
#[derive(Debug, Clone)]
pub struct ProbeHead {
    /// The `operations::Route` variant of the `GET` operation.
    pub route: String,
    /// The template, which is the request path as well: the operation takes no parameter, so
    /// it names no variable.
    pub path: String,
}

/// One operation: the trait method the double implements, and the request the driver sends.
#[derive(Debug, Clone)]
pub struct ProbeOp {
    /// The generated client method, which is also the trait method's name.
    pub method: String,
    /// The `operations::Route` variant naming this operation's route: what the reflection has to
    /// resolve from the method and matched template a layer records when the driver's request
    /// goes through the generated router.
    pub route: String,
    /// Whether exercising the operation deliberately calls deprecated API.
    pub deprecated: bool,
    /// The grouped parameter arguments, in trait-signature order.
    pub groups: Vec<ProbeGroup>,
    /// The body argument, when the operation takes one.
    pub body: Option<ProbeBody>,
    /// The params struct's name, spelled from the test file (`client::FooParams`), when the
    /// operation takes any input.
    pub params: Option<String>,
    /// One field of the params literal per parameter; `body` follows when there is one.
    pub inputs: Vec<ProbeInput>,
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

/// One field the driver writes in the params literal.
#[derive(Debug, Clone)]
pub struct ProbeInput {
    /// The field's name, which is the parameter's Rust name.
    pub field: String,
    /// Whether writing the field deliberately exercises deprecated API.
    pub deprecated: bool,
    /// The Rust path of the parameter's type.
    pub ty: String,
    /// A JSON value of that type, or `None` for a field the driver deliberately leaves unset —
    /// an optional parameter whose name the wire erases, which no request can usefully carry.
    pub json: Option<String>,
    /// Whether the document requires the parameter: a required field takes the value bare, an
    /// optional one wrapped in `Some`.
    pub required: bool,
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

    let planned: Vec<(&OperationContract, ProbeOp)> = model
        .operations()
        .iter()
        .filter(|operation| operation.registrable.is_some())
        .map(|operation| (operation, plan(operation, &contracts, config)))
        .collect();
    // The first driven `GET` that takes no parameter: the template then names no variable, so
    // the request path is the template itself, and the double asserts nothing about what
    // arrived — a hand-built `HEAD` carries none of the optional inputs the driver would set.
    // Not one whose template also declares a `HEAD`: axum dispatches to that handler first, and
    // `from_matched` rightly names that route, so the fallback this request exists to exercise
    // would never run.
    let head = planned
        .iter()
        .find(|(operation, planned)| {
            planned.skip.is_none()
                && operation.method == crate::api::Method::Get
                && operation.params.is_empty()
                && !model.operations().iter().any(|other| {
                    other.registrable.is_some()
                        && other.method == crate::api::Method::Head
                        && other.path == operation.path
                })
        })
        .map(|(operation, planned)| ProbeHead {
            route: planned.route.clone(),
            path: operation.path.to_string(),
        });
    let operations = planned.into_iter().map(|(_, planned)| planned).collect();
    Ok(Probe { operations, head })
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

    let inputs = probe_inputs(operation, contracts, config, &mut skip);

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
        route: render::operations::variant(operation).to_string(),
        deprecated: operation.docs.deprecated,
        groups,
        body,
        params: render::client::takes_params(operation)
            .then(|| format!("client::{}", render::client::params_name(operation))),
        inputs,
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
        ResponseBody::Json { ty, .. } => ProbeValue::Json {
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

/// One synthesized params field per parameter, recording a skip where none can be.
fn probe_inputs(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
    skip: &mut Option<String>,
) -> Vec<ProbeInput> {
    let mut inputs = Vec::new();
    for param in &operation.params {
        if param.style.erased_by_explosion() {
            // The wire erases the parameter's name, so the server can never see it. Optional: the
            // driver writes the field as `None` — the literal must still name it — and the
            // operation is probeable. Required: the handler rejects every request without it, so
            // the operation cannot cross the wire at all — a named skip, not a red test and not a
            // silent cap.
            if param.required {
                skip.get_or_insert(format!(
                    "parameter `{}` is a required exploded `form` object, whose name the wire \
                     erases; no request can carry it in a form the server can read",
                    param.wire_name
                ));
                continue;
            }
            let (ty, _) = test_type(&render::types::type_path(&param.ty, contracts, config));
            inputs.push(ProbeInput {
                field: param.rust_name.as_str().to_owned(),
                // A bare `None` spells neither the type nor anything deprecated — a params
                // field carries its deprecation in prose — so there is nothing to exempt.
                deprecated: false,
                ty,
                json: None,
                required: false,
            });
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
                inputs.push(ProbeInput {
                    field: param.rust_name.as_str().to_owned(),
                    // Only the spelled *type* can lint: a params field carries its deprecation
                    // in prose, and the field name itself never warns.
                    deprecated: deprecated_type,
                    ty,
                    json: Some(value.to_string()),
                    required: param.required,
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
    inputs
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
        // The object form an `Upload` serializes to, every field present, so the probe
        // round-trips exactly.
        TypeRef::Upload => json!({
            "bytes": "cHJvYmU=",
            "filename": "probe.bin",
            "content_type": "application/octet-stream",
        }),
        TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Boxed(inner) => {
            synthesize(inner, contracts, visiting)?
        }
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
        // One synthesis for both tag styles. A consumed union's variant lacks the member, so the
        // insert adds it; a carried union's variant declares it, so the insert overwrites whatever
        // placeholder the member synthesized as — either way the payload names the variant it is.
        ContractKind::TaggedEnum { tag, variants }
        | ContractKind::CarriedTagEnum { tag, variants } => {
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

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::json;

    use crate::config::Config;

    fn plan(paths: &serde_json::Value) -> eyre::Result<super::Probe> {
        let document = json!({"openapi": "3.1.0", "paths": paths});
        Ok(super::probe(
            document.to_string().as_bytes(),
            &Config::default(),
        )?)
    }

    /// axum serves `HEAD /a` with the `HEAD` handler `/a` declares and `HEAD /b` with the
    /// `GET` one, and only the second is the fallback the case exists to exercise.
    #[test_util::test]
    fn the_head_case_avoids_a_template_that_declares_its_own_head() {
        let with_alternative = plan(&json!({
            "/a": {
                "get": {"operationId": "getA", "responses": {"200": {"description": "ok"}}},
                "head": {"operationId": "headA", "responses": {"200": {"description": "ok"}}},
            },
            "/b": {"get": {"operationId": "getB", "responses": {"200": {"description": "ok"}}}},
        }))?;
        assert_eq!(
            with_alternative
                .head
                .as_ref()
                .map(|head| (head.route.as_str(), head.path.as_str())),
            Some(("GetB", "/b"))
        );
        let routes: Vec<&str> = with_alternative
            .operations
            .iter()
            .map(|operation| operation.route.as_str())
            .collect();
        assert_eq!(routes, ["GetA", "HeadA", "GetB"]);

        // With nothing but the paired template, the case is not driven and the plan says so.
        let alone = plan(&json!({
            "/a": {
                "get": {"operationId": "getA", "responses": {"200": {"description": "ok"}}},
                "head": {"operationId": "headA", "responses": {"200": {"description": "ok"}}},
            },
        }))?;
        assert!(alone.head.is_none());
    }

    /// A `GET` that takes any parameter is not the target either: a hand-built `HEAD` carries
    /// none of the inputs the double would assert arrived.
    #[test_util::test]
    fn the_head_case_takes_a_parameterless_get() {
        let probe = plan(&json!({
            "/a": {"get": {
                "operationId": "getA",
                "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
                "responses": {"200": {"description": "ok"}},
            }},
            "/b": {"get": {"operationId": "getB", "responses": {"200": {"description": "ok"}}}},
        }))?;
        assert_eq!(
            probe.head.as_ref().map(|head| head.route.as_str()),
            Some("GetB")
        );
    }
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
