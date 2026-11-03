//! A response's declared headers: checked, named, typed, and judged writable or not.
//!
//! A header gets a typed field only where the description says exactly what the wire carries.
//! Every other declared header is left to the arm's raw headers with a diagnostic rather than
//! half-supported, because a typed field that misreads the field it names is worse than none: the
//! raw headers still reach it, and they claim nothing about it.
//!
//! The typed set is what the `simple` style writes and reads back unchanged:
//!
//! - a scalar: a boolean, a number, a string, a formatted string such as a date or a UUID, or a
//!   string enum;
//! - a value whose schema says nothing about it, typed as its text, because text is all `simple`
//!   says of it;
//! - a list of scalars, comma-separated in one field;
//! - any value declared with a JSON `content` that cannot be `null`, written as one JSON
//!   document.
//!
//! Left to the raw headers are objects and maps, whose `k,v` spelling cannot keep a member apart
//! from its value once either holds a comma, lists of anything but scalars, unions and tuples,
//! any style but `simple`, a JSON document that may be `null`, which it spells apart from being
//! absent, `Set-Cookie`, which repeats once per cookie and must never be folded into one
//! comma-separated value, and the fields the transport owns, which frame the message or manage
//! the connection.
//! A declared `Content-Type` is ignored outright, as OpenAPI says, because the arm's media type
//! writes it.

use std::collections::BTreeSet;

use super::Build;
use crate::api::{ResponseHeader, StyleContract, style};
use crate::contract::{ContractKind, Contracts, Namer, RustIdent, TypeContract, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Header, Parameter, Response, ignored_response_header};
use crate::shape::Docs;

impl Build<'_> {
    /// The headers a response declares that get a typed field, ordered by wire name.
    ///
    /// `at` is the response's position, which is where the headers are reported, whether the
    /// response is written there or referenced from `components.responses`.
    pub(super) fn headers(
        &self,
        response: &Response,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Vec<ResponseHeader> {
        let at = at.child("headers");
        let mut used = Namer::default();
        // The group's own field for everything it does not type.
        let _ = used.unique(RustIdent::field("raw"));
        // Lowercased, because HTTP field names are case-insensitive and two spellings are one
        // field on the wire.
        let mut declared = BTreeSet::new();
        let mut headers = Vec::new();
        for (name, node) in response.headers.iter().flatten() {
            let at = at.child(name.clone());
            if ignored_response_header(name) {
                continue;
            }
            // A dangling reference was reported where it was resolved, and claims no name.
            let Some(header) = self.resolved.header(node) else {
                continue;
            };
            // Never repaired into a name that is one: that would be a different header.
            let typing = if !is_field_name(name) {
                Err(
                    "the header's name is not an HTTP field name, so no response can carry it \
                     under that name; it gets no typed field"
                        .to_owned(),
                )
            } else if !declared.insert(name.to_ascii_lowercase()) {
                Err(
                    "the response declares this header again in another case, and HTTP field \
                     names are case-insensitive; the declaration whose name sorts first keeps the \
                     typed field and this one is ignored"
                        .to_owned(),
                )
            } else {
                self.typing(name, header)
            };
            let Typing { ty, style, leaf } = match typing {
                Ok(typing) => typing,
                Err(detail) => {
                    ctx.report(Diagnostic::new(
                        BreakageClass::UntypedResponseHeader,
                        Action::Degrade,
                        at,
                        detail,
                    ));
                    continue;
                }
            };
            headers.push(ResponseHeader {
                rust_name: used.unique(RustIdent::field(name)),
                wire_name: name.clone(),
                ty,
                required: header.required.unwrap_or(false),
                style,
                spelled: leaf == Leaf::Spelled,
                docs: Docs {
                    description: header.description.clone(),
                    deprecated: header.deprecated.unwrap_or(false),
                    ..Docs::default()
                },
                origin: at,
            });
        }
        headers
    }

    /// How a validly named header is typed, or why it gets no typed field.
    fn typing(&self, name: &str, header: &Header) -> Result<Typing, String> {
        if name.eq_ignore_ascii_case("set-cookie") {
            return Err(
                "`Set-Cookie` repeats once per cookie and must never be folded into one \
                 comma-separated value, which is how `simple` writes a list; it gets no typed \
                 field and is left to the raw headers"
                    .to_owned(),
            );
        }
        if transport_owned(name) {
            return Err(
                "the header frames the message or manages the connection, which the HTTP \
                 transport does for every response it sends, so a value a handler wrote would \
                 contradict the framing it chooses or be dropped on the way out; it gets no \
                 typed field and is left to the raw headers"
                    .to_owned(),
            );
        }
        let parameter = as_parameter(name, header);
        let (ty, shape) = self.param_type(&parameter);
        let style = style::classify(&parameter, shape).map_err(|undefined| {
            format!(
                "{}; the header gets no typed field and is left to the raw headers",
                undefined.detail
            )
        })?;
        // One JSON document is one value on the wire, whatever its type, and its text is parsed
        // whole.
        let leaf = if style.content() {
            // A document that may be `null` spells it, which is not the header being absent, and
            // one optional field cannot hold the two apart. Arbitrary JSON and `null` itself admit
            // it as much as a nullable wrapper does.
            let document = underlying(&ty, self.contracts);
            if document.nullable || matches!(document.ty, TypeRef::Value | TypeRef::Unit) {
                return Err(
                    "the header is one JSON document whose schema admits `null`, which it \
                     spells apart from being absent, and one optional field cannot tell the two \
                     apart; it gets no typed field and is left to the raw headers"
                        .to_owned(),
                );
            }
            Leaf::Text
        } else {
            writable(&ty, self.contracts).map_err(|unwritable| {
                format!(
                    "the header's type is {unwritable}, which `simple` cannot write so that it \
                     reads back unchanged; it gets no typed field and is left to the raw headers"
                )
            })?
        };
        let ty = match leaf {
            // Text is all `simple` says of a value its schema says nothing about, so the field
            // holds the text: `true` and `7` stay the strings a handler wrote rather than reading
            // back as a boolean and a number.
            Leaf::Unconstrained => as_text(&ty, self.contracts),
            Leaf::Text | Leaf::Spelled => self.contracts.read_form(non_null(&ty)),
        };
        Ok(Typing { ty, style, leaf })
    }
}

/// How a header with a typed field is typed: the field's type, its style, and how its scalars'
/// text reads back.
struct Typing {
    ty: TypeRef,
    style: StyleContract,
    leaf: Leaf,
}

/// A header as the parameter OpenAPI defines it to be: a Header Object is a Parameter Object
/// without `name` and `in`, which the response's map key and the `header` location supply.
fn as_parameter(name: &str, header: &Header) -> Parameter {
    Parameter {
        name: Some(name.to_owned()),
        location: Some("header".to_owned()),
        required: header.required,
        style: header.style.clone(),
        explode: header.explode,
        schema: header.schema,
        content: header.content.clone(),
        ..Parameter::default()
    }
}

/// The fields the HTTP transport writes for every response, which no handler can keep true of a
/// message it neither frames nor sends.
///
/// The framing: `Content-Length` and `Transfer-Encoding` say where the body ends (RFC 9112,
/// section 6), and `Trailer` announces fields that follow a chunked body (RFC 9110, section
/// 6.6.2).
/// A value a handler wrote would contradict the framing the transport chose for the body it
/// actually sends, which corrupts the stream rather than describing it.
///
/// The connection: `Connection` and the fields RFC 9110, section 7.6.1, names as specific to one
/// connection, `Keep-Alive`, `Proxy-Connection`, `TE` and `Upgrade`.
/// They describe one hop rather than the response, and HTTP/2 forbids them outright (RFC 9113,
/// section 8.2.2), so a transport drops or replaces whatever a handler wrote.
const TRANSPORT_OWNED: [&str; 8] = [
    "content-length",
    "transfer-encoding",
    "trailer",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "upgrade",
];

/// Whether the transport owns the field of this name, as [`TRANSPORT_OWNED`] says.
fn transport_owned(name: &str) -> bool {
    TRANSPORT_OWNED
        .iter()
        .any(|owned| name.eq_ignore_ascii_case(owned))
}

/// Whether a name is an HTTP field name: one or more `tchar`s (RFC 9110, section 5.1).
fn is_field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// The type without the `null` a header has no spelling for: absent is the only way a header
/// says nothing.
fn non_null(ty: &TypeRef) -> &TypeRef {
    match ty {
        TypeRef::Option(inner) | TypeRef::Presence(inner) => non_null(inner),
        other => other,
    }
}

/// A type with its wrappers and aliases looked through.
struct Underlying<'a> {
    ty: &'a TypeRef,
    /// Whether one of the wrappers admits `null`.
    nullable: bool,
}

fn underlying<'a>(ty: &'a TypeRef, contracts: &'a Contracts) -> Underlying<'a> {
    let mut current = ty;
    let mut nullable = false;
    // A type met again wraps nothing but itself, and the walk ends at it.
    let mut followed = BTreeSet::new();
    loop {
        current = match current {
            TypeRef::Option(inner) | TypeRef::Presence(inner) => {
                nullable = true;
                inner
            }
            TypeRef::Boxed(inner) => inner,
            TypeRef::Named(index) if followed.insert(*index) => {
                match contracts.get(*index).map(TypeContract::kind) {
                    Some(
                        ContractKind::Newtype { inner } | ContractKind::Alias { target: inner },
                    ) => inner,
                    _ => current,
                }
            }
            _ => {
                return Underlying {
                    ty: current,
                    nullable,
                };
            }
        };
    }
}

/// How a scalar's text reads back into its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaf {
    /// The text is the value: a string, a formatted string, a string enum.
    Text,
    /// The text spells the value as JSON: a boolean or a number, whose text says which it is.
    Spelled,
    /// A value its schema says nothing about, whose text cannot say what it is: `true` is a
    /// boolean or the string a handler wrote, so the value is typed as its text.
    Unconstrained,
}

/// The text a value of this type is, keeping the list it may be: what an unconstrained scalar,
/// or a list of them, is typed as.
fn as_text(ty: &TypeRef, contracts: &Contracts) -> TypeRef {
    match underlying(ty, contracts).ty {
        TypeRef::Vec(_) => TypeRef::Vec(Box::new(TypeRef::String)),
        TypeRef::Array(_, length) => TypeRef::Array(Box::new(TypeRef::String), *length),
        _ => TypeRef::String,
    }
}

/// How `simple` reads back the scalars of a value of this type, when it writes the value so that
/// it reads back unchanged, or what the type is instead.
fn writable(ty: &TypeRef, contracts: &Contracts) -> Result<Leaf, &'static str> {
    let Underlying { ty, .. } = underlying(ty, contracts);
    let (TypeRef::Vec(element) | TypeRef::Array(element, _)) = ty else {
        return scalar(ty, contracts);
    };
    let element = underlying(element, contracts);
    if element.nullable {
        return Err("a list whose elements may be `null`");
    }
    scalar(element.ty, contracts).map_err(|_| "a list of compound values")
}

/// How a looked-through type's text reads back, when it is a scalar, or what it is instead.
fn scalar(ty: &TypeRef, contracts: &Contracts) -> Result<Leaf, &'static str> {
    match ty {
        TypeRef::Bool | TypeRef::Integer(_) | TypeRef::F64 => Ok(Leaf::Spelled),
        TypeRef::Value => Ok(Leaf::Unconstrained),
        TypeRef::String | TypeRef::Format(_) => Ok(Leaf::Text),
        TypeRef::Named(index) => match contracts.get(*index).map(TypeContract::kind) {
            Some(ContractKind::StringEnum { .. }) => Ok(Leaf::Text),
            Some(ContractKind::Struct { .. }) => Err("an object"),
            Some(
                ContractKind::Enum { .. }
                | ContractKind::TaggedEnum { .. }
                | ContractKind::CarriedTagEnum { .. }
                | ContractKind::KeyedEnum { .. },
            ) => Err("a union"),
            Some(ContractKind::Tuple { .. }) => Err("a tuple"),
            // What `underlying` stopped at: a wrapper of nothing but itself.
            Some(ContractKind::Newtype { .. } | ContractKind::Alias { .. }) | None => {
                Err("a type that names nothing but itself")
            }
        },
        TypeRef::Map(_) => Err("a map"),
        TypeRef::Vec(_) | TypeRef::Array(..) => Err("a list of lists"),
        TypeRef::Tuple(_) => Err("a tuple"),
        TypeRef::Unit => Err("`null`"),
        TypeRef::Upload => Err("a file"),
        // Looked through by `underlying` before this is asked.
        TypeRef::Option(_) | TypeRef::Presence(_) | TypeRef::Boxed(_) => {
            Err("a value that may be `null`")
        }
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use crate::api::ResponseHeader;
    use crate::api::tests::{model_of, with_paths};
    use crate::contract::TypeRef;
    use crate::diag::{Action, Diagnostic};

    /// The typed headers of the one response a one-operation document declares, with what was
    /// said about the rest.
    fn headers_of(headers: &Value) -> eyre::Result<(Vec<ResponseHeader>, Vec<Diagnostic>)> {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/x": {"get": {
                "operationId": "getX",
                "responses": {"200": {"description": "ok", "headers": headers}},
            }},
        })))?;
        let arm = model
            .operations()
            .first()
            .and_then(|operation| operation.responses.arms.first())
            .ok_or_eyre("the one arm")?;
        Ok((arm.headers.clone(), diagnostics))
    }

    /// The details of every `untyped-response-header` diagnostic, by where it points.
    fn untyped(diagnostics: &[Diagnostic]) -> Vec<(String, &str)> {
        diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::UntypedResponseHeader)
            .map(|found| (found.location().to_string(), found.detail()))
            .collect()
    }

    /// A wire name keeps its spelling, and its Rust name is allocated deterministically: snake
    /// case, a keyword escaped, a leading digit guarded, `raw` left to the group, and two names
    /// that sanitize alike told apart by a suffix.
    #[test_util::test]
    fn each_header_gets_a_field_name_the_group_can_hold() {
        let string = json!({"schema": {"type": "string"}});
        let (headers, diagnostics) = headers_of(&json!({
            "Retry-After": string,
            "type": string,
            "1st-Hop": string,
            "Raw": string,
            "X-Rate": string,
            "X_Rate": string,
        }))?;
        let names: Vec<(&str, &str)> = headers
            .iter()
            .map(|header| (header.wire_name.as_str(), header.rust_name.as_str()))
            .collect();
        // Byte order, so `1st-Hop` leads and the two `X` spellings sit together; the second to
        // sanitize to `x_rate` takes the suffix.
        assert_eq!(
            names,
            [
                ("1st-Hop", "_1st_hop"),
                ("Raw", "raw2"),
                ("Retry-After", "retry_after"),
                ("X-Rate", "x_rate"),
                ("X_Rate", "x_rate2"),
                ("type", "type_"),
            ]
        );
        assert_eq!(untyped(&diagnostics), []);
    }

    /// Two spellings HTTP does not tell apart are one field: the first in byte order keeps the
    /// typed field and the other is reported, never renamed.
    #[test_util::test]
    fn a_header_declared_twice_in_two_cases_is_typed_once_and_reported() {
        let (headers, diagnostics) = headers_of(&json!({
            "x-rate": {"schema": {"type": "integer"}},
            "X-Rate": {"schema": {"type": "string"}},
        }))?;
        let kept: Vec<(&str, &TypeRef)> = headers
            .iter()
            .map(|header| (header.wire_name.as_str(), &header.ty))
            .collect();
        assert_eq!(kept, [("X-Rate", &TypeRef::String)]);
        let reported = untyped(&diagnostics);
        assert_eq!(reported.len(), 1, "{reported:?}");
        let (location, detail) = &reported[0];
        assert_eq!(location, "/paths/~1x/get/responses/200/headers/x-rate");
        assert!(detail.contains("case-insensitive"), "{detail}");
    }

    /// A declared `Content-Type` is ignored as OpenAPI says, in any case, and silently: the
    /// arm's media type writes it, so nothing is lost.
    #[test_util::test]
    fn a_declared_content_type_is_ignored() {
        let (headers, diagnostics) = headers_of(&json!({
            "Content-Type": {"schema": {"type": "string"}},
            "content-type": {"schema": {"type": "string"}},
            "Location": {"schema": {"type": "string"}},
        }))?;
        let names: Vec<&str> = headers
            .iter()
            .map(|header| header.wire_name.as_str())
            .collect();
        assert_eq!(names, ["Location"]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// The typed set: scalars, formats, string enums, lists of them, a value or list its schema
    /// says nothing about, a header with no schema, and a JSON document that cannot be `null`.
    /// A header has no `null`, so a nullable declaration is typed as what it makes nullable.
    #[test_util::test]
    fn scalars_lists_of_scalars_and_json_documents_are_typed() {
        let mut document = with_paths(json!({
            "/x": {"get": {
                "operationId": "getX",
                "responses": {"200": {"description": "ok", "headers": {
                    "A-Flag": {"schema": {"type": "boolean"}},
                    "B-Count": {"schema": {"type": "integer"}, "required": true},
                    "C-Ratio": {"schema": {"type": ["number", "null"]}},
                    "D-When": {"schema": {"type": "string", "format": "date-time"}},
                    "E-Mode": {"schema": {"$ref": "#/components/schemas/Mode"}},
                    "F-Ids": {"schema": {"type": "array", "items": {"type": "integer"}}},
                    "G-Modes": {"schema": {"type": "array", "items": {"$ref": "#/components/schemas/Mode"}}},
                    "H-Any": {"schema": {}},
                    "I-Bare": {"description": "no schema at all"},
                    "J-Shape": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Shape"}}}},
                    "K-Token": {"schema": {"$ref": "#/components/schemas/Token"}},
                    "L-Anys": {"schema": {"type": "array", "items": {}}},
                    "N-Opaque": {"schema": {"$ref": "#/components/schemas/Opaque"}},
                }}},
            }},
        }));
        document["components"] = json!({"schemas": {
            "Mode": {"type": "string", "enum": ["fast", "slow"]},
            "Shape": {"type": "object", "properties": {"side": {"type": "integer"}}},
            "Token": {"type": "string", "minLength": 4},
            "Opaque": {"description": "anything at all"},
        }});
        let (model, diagnostics) = model_of(document)?;
        let arm = model
            .operations()
            .first()
            .and_then(|operation| operation.responses.arms.first())
            .ok_or_eyre("the one arm")?;
        let typed: Vec<&str> = arm
            .headers
            .iter()
            .map(|header| header.wire_name.as_str())
            .collect();
        assert_eq!(
            typed,
            [
                "A-Flag", "B-Count", "C-Ratio", "D-When", "E-Mode", "F-Ids", "G-Modes", "H-Any",
                "I-Bare", "J-Shape", "K-Token", "L-Anys", "N-Opaque",
            ]
        );
        let header = |name: &str| arm.headers.iter().find(|header| header.wire_name == name);
        let ratio = header("C-Ratio").ok_or_eyre("C-Ratio")?;
        assert_eq!(ratio.ty, TypeRef::F64);
        assert!(!ratio.required);
        assert!(header("B-Count").ok_or_eyre("B-Count")?.required);
        assert!(header("F-Ids").ok_or_eyre("F-Ids")?.style.array());
        assert_eq!(header("I-Bare").ok_or_eyre("I-Bare")?.ty, TypeRef::String);
        assert!(header("J-Shape").ok_or_eyre("J-Shape")?.style.content());
        assert_eq!(
            header("A-Flag").ok_or_eyre("A-Flag")?.origin.to_string(),
            "/paths/~1x/get/responses/200/headers/A-Flag"
        );
        assert_eq!(untyped(&diagnostics), []);

        // A boolean and a number read back as the JSON their text spells, a list as its elements
        // do; a string of any kind is its text, and a JSON document is parsed whole.
        let spelled: Vec<&str> = arm
            .headers
            .iter()
            .filter(|header| header.spelled)
            .map(|header| header.wire_name.as_str())
            .collect();
        assert_eq!(spelled, ["A-Flag", "B-Count", "C-Ratio", "F-Ids"]);

        // A value its schema says nothing about is typed as the text `simple` carries, inline or
        // named, alone or as a list's elements, so `true` stays the string it was written as.
        let ty = |name: &str| header(name).map(|header| &header.ty);
        assert_eq!(ty("H-Any"), Some(&TypeRef::String));
        assert_eq!(ty("N-Opaque"), Some(&TypeRef::String));
        assert_eq!(ty("L-Anys"), Some(&TypeRef::Vec(Box::new(TypeRef::String))));
    }

    /// A shape `simple` cannot write so that it reads back unchanged is reported and left to the
    /// raw headers, as is a style other than `simple`, `Set-Cookie`, and a name HTTP cannot carry.
    #[test_util::test]
    fn a_header_simple_cannot_write_faithfully_is_left_to_the_raw_headers() {
        let (headers, diagnostics) = headers_of(&json!({
            "A-Object": {"schema": {"type": "object", "properties": {"k": {"type": "string"}}}},
            "B-Map": {"schema": {"type": "object", "additionalProperties": {"type": "string"}}},
            "C-Nested": {"schema": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}}},
            "D-Union": {"schema": {"oneOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "integer"},
            ]}},
            "E-Nulls": {"schema": {"type": "array", "items": {"type": ["string", "null"]}}},
            "F-Form": {"style": "form", "schema": {"type": "string"}},
            "Set-Cookie": {"schema": {"type": "string"}},
            "Bad Name": {"schema": {"type": "string"}},
            "G-Fine": {"schema": {"type": "string"}},
        }))?;
        let names: Vec<&str> = headers
            .iter()
            .map(|header| header.wire_name.as_str())
            .collect();
        assert_eq!(names, ["G-Fine"]);
        let reported = untyped(&diagnostics);
        let at = |name: &str| format!("/paths/~1x/get/responses/200/headers/{name}");
        let detail = |name: &str| {
            reported
                .iter()
                .find(|(location, _)| *location == at(name))
                .map(|(_, detail)| *detail)
                .ok_or_eyre(format!("{name} is reported"))
        };
        assert!(detail("A-Object")?.contains("an object"));
        assert!(detail("B-Map")?.contains("a map"));
        assert!(detail("C-Nested")?.contains("compound"));
        assert!(detail("D-Union")?.contains("a union"));
        assert!(detail("E-Nulls")?.contains("`null`"));
        assert!(detail("F-Form")?.contains("style: form"));
        assert!(detail("Set-Cookie")?.contains("folded"));
        assert!(detail("Bad Name")?.contains("not an HTTP field name"));
        assert_eq!(reported.len(), 8, "{reported:#?}");
        assert!(
            diagnostics
                .iter()
                .filter(|found| found.class() == crate::BreakageClass::UntypedResponseHeader)
                .all(|found| found.action() == Action::Degrade)
        );
    }

    /// The fields the transport owns are reported and left to the raw headers in any case,
    /// whatever their schema: a handler's `Content-Length` would contradict the body the
    /// transport frames.
    #[test_util::test]
    fn a_header_the_transport_owns_is_left_to_the_raw_headers() {
        let integer = json!({"schema": {"type": "integer"}});
        let string = json!({"schema": {"type": "string"}});
        let (headers, diagnostics) = headers_of(&json!({
            "Content-Length": integer,
            "transfer-encoding": string,
            "Trailer": string,
            "Connection": string,
            "Keep-Alive": string,
            "Proxy-Connection": string,
            "TE": string,
            "Upgrade": string,
            "Location": string,
        }))?;
        let names: Vec<&str> = headers
            .iter()
            .map(|header| header.wire_name.as_str())
            .collect();
        assert_eq!(names, ["Location"]);
        // One finding, folded across the eight headers it stands for.
        let [owned] = diagnostics.as_slice() else {
            eyre::bail!("one folded finding: {diagnostics:#?}");
        };
        assert_eq!(owned.class(), crate::BreakageClass::UntypedResponseHeader);
        assert!(owned.detail().contains("frames the message"), "{owned}");
        assert_eq!(owned.occurrences().get(), 8, "{owned}");
        assert_eq!(
            owned.location().to_string(),
            "/paths/~1x/get/responses/200/headers/Connection"
        );
    }

    /// A JSON document whose schema admits `null` spells it apart from being absent, which one
    /// optional field cannot, so it is left to the raw headers, inline or through a reference,
    /// and so is arbitrary JSON and `null` itself; one that cannot be `null` is typed.
    #[test_util::test]
    fn a_json_document_that_may_be_null_is_left_to_the_raw_headers() {
        let mut document = with_paths(json!({
            "/x": {"get": {
                "operationId": "getX",
                "responses": {"200": {"description": "ok", "headers": {
                    "A-Count": {"content": {"application/json": {
                        "schema": {"type": ["integer", "null"]},
                    }}},
                    "B-Shape": {"content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/MaybeShape"},
                    }}},
                    "C-Count": {"content": {"application/json": {
                        "schema": {"type": "integer"},
                    }}},
                    "D-Any": {"content": {"application/json": {"schema": {}}}},
                    "E-Null": {"content": {"application/json": {"schema": {"type": "null"}}}},
                }}},
            }},
        }));
        document["components"] = json!({"schemas": {
            "MaybeShape": {
                "type": ["object", "null"],
                "properties": {"side": {"type": "integer"}},
            },
        }});
        let (model, diagnostics) = model_of(document)?;
        let arm = model
            .operations()
            .first()
            .and_then(|operation| operation.responses.arms.first())
            .ok_or_eyre("the one arm")?;
        let typed: Vec<(&str, &TypeRef)> = arm
            .headers
            .iter()
            .map(|header| (header.wire_name.as_str(), &header.ty))
            .collect();
        assert_eq!(
            typed,
            [("C-Count", &TypeRef::Integer(crate::shape::IntType::I64))]
        );
        // One finding, folded across every document that may be `null`: a nullable wrapper,
        // inline or named, arbitrary JSON, and `null` itself.
        let nullable: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::UntypedResponseHeader)
            .collect();
        let [nullable] = nullable.as_slice() else {
            eyre::bail!("one folded finding: {diagnostics:#?}");
        };
        assert!(nullable.detail().contains("admits `null`"), "{nullable}");
        assert_eq!(nullable.occurrences().get(), 4, "{nullable}");
        let at = |name: &str| format!("/paths/~1x/get/responses/200/headers/{name}");
        assert_eq!(nullable.location().to_string(), at("A-Count"));
        let related: Vec<String> = nullable.related().iter().map(ToString::to_string).collect();
        assert_eq!(related, [at("B-Shape"), at("D-Any"), at("E-Null")]);
    }

    /// A header written once in `components.headers` and referenced is typed like an inline one,
    /// and reported at the response that references it.
    #[test_util::test]
    fn a_referenced_header_is_typed_at_the_response_that_names_it() {
        let mut document = with_paths(json!({
            "/x": {"get": {
                "operationId": "getX",
                "responses": {"200": {"description": "ok", "headers": {
                    "X-Trace": {"$ref": "#/components/headers/Trace"},
                    "X-Gone": {"$ref": "#/components/headers/Missing"},
                }}},
            }},
        }));
        document["components"] = json!({"headers": {
            "Trace": {"description": "the trace", "deprecated": true, "schema": {"type": "string"}},
        }});
        let (model, _) = model_of(document)?;
        let arm = model
            .operations()
            .first()
            .and_then(|operation| operation.responses.arms.first())
            .ok_or_eyre("the one arm")?;
        let [header] = arm.headers.as_slice() else {
            eyre::bail!("one typed header, the dangling one claiming nothing: {arm:#?}");
        };
        assert_eq!(header.wire_name, "X-Trace");
        assert_eq!(header.docs.description.as_deref(), Some("the trace"));
        assert!(header.docs.deprecated);
        assert_eq!(
            header.origin.to_string(),
            "/paths/~1x/get/responses/200/headers/X-Trace"
        );
    }

    #[test_util::test]
    fn a_field_name_is_a_token() {
        assert!(super::is_field_name("X-Rate_Limit.v2~"));
        assert!(!super::is_field_name(""));
        assert!(!super::is_field_name("X Rate"));
        assert!(!super::is_field_name("X:Rate"));
        assert!(!super::is_field_name("Grüße"));
    }
}
