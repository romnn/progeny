//! The operation's parameters: merged, named, typed, and judged serializable or not.

use std::collections::BTreeMap;

use super::Build;
use crate::api::ParamContract;
use crate::api::style::{self, ParamShape};
use crate::contract::{ContractKind, Contracts, Namer, RustIdent, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Operation, Parameter, PathItem};
use crate::shape::Docs;

impl Build<'_> {
    /// The operation's parameters, path-level ones included.
    ///
    /// Returns `None` when the operation cannot be called at all.
    pub(super) fn params(
        &mut self,
        operation: &Operation,
        item: &PathItem,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<Vec<ParamContract>> {
        // An operation-level parameter overrides a path-level one with the same (name, in), which
        // is the specification's own rule; a map keyed by the pair implements it exactly.
        let mut merged: BTreeMap<(String, String), &Parameter> = BTreeMap::new();
        let listed = item
            .parameters
            .iter()
            .flatten()
            .chain(operation.parameters.iter().flatten());
        for node in listed {
            let Some(parameter) = self.resolved.parameter(node) else {
                continue;
            };
            let name = parameter.name.clone().unwrap_or_default();
            let location = parameter.location.clone().unwrap_or_default();
            merged.insert((name, location), parameter);
        }

        let mut used = Namer::default();
        // The params struct reserves this: `body` is the field the request body lives in, and a
        // parameter with the same name would declare the field twice. The other interface names —
        // `client`, `params`, `headers`, and the request's methods — live on the request struct,
        // which holds no parameter fields, so parameters keep those names freely. Reserved here
        // rather than in the renderer because which names the interface occupies is a fact about
        // the interface, and a renderer that renamed a parameter would be a renderer making a
        // decision.
        let _ = used.unique(RustIdent::field("body"));
        let mut params = Vec::new();
        for ((name, _), parameter) in merged {
            // A path parameter is required whatever the document says — the URL has a hole in it
            // either way — so the record carries the flag OpenAPI obliges the document to write,
            // and both halves of the interface treat the parameter as the URL does.
            let required = parameter.required.unwrap_or(false)
                || parameter.location.as_deref() == Some("path");
            let (ty, shape) = self.param_type(parameter);
            let style = match style::classify(parameter, shape) {
                Ok(style) => style,
                Err(undefined) => {
                    ctx.report(Diagnostic::new(
                        BreakageClass::QuerySerializationStyle,
                        Action::Degrade,
                        at.child("parameters").child(name.clone()),
                        if required {
                            format!(
                                "{}; the parameter is required, so the operation is skipped rather \
                                 than called without it",
                                undefined.detail
                            )
                        } else {
                            format!(
                                "{}; the parameter is optional, so it is left out and the rest of \
                                 the operation is generated",
                                undefined.detail
                            )
                        },
                    ));
                    if required {
                        return None;
                    }
                    continue;
                }
            };
            // Defined by OpenAPI and writable by the client — and still a degradation, because
            // the wire erases the parameter's name: an exploded `form` object's members travel
            // as their own query keys, and no server can tell them from any other member. The
            // handler's field is permanently absent; the parameter is kept because the calling
            // side is whole.
            if style.erased_by_explosion() {
                ctx.report(Diagnostic::new(
                    BreakageClass::QuerySerializationStyle,
                    Action::Degrade,
                    at.child("parameters").child(name.clone()),
                    if required {
                        "the parameter is an exploded `form` object, whose members travel under \
                         their own names with the parameter's name erased, so the generated \
                         handler can never see it; it is required, so the handler rejects every \
                         request and only the calling side of this operation is servable"
                    } else {
                        "the parameter is an exploded `form` object, whose members travel under \
                         their own names with the parameter's name erased, so the generated \
                         handler can never see it; it is optional, so the handler runs with the \
                         member always absent"
                    },
                ));
            }
            params.push(ParamContract {
                rust_name: used.unique(RustIdent::field(&name)),
                wire_name: name,
                style,
                required,
                ty,
                docs: Docs {
                    description: parameter.description.clone(),
                    deprecated: parameter.deprecated.unwrap_or(false),
                    ..Docs::default()
                },
            });
        }
        Some(params)
    }

    /// A parameter's type, and which of the three serialization shapes it has.
    fn param_type(&self, parameter: &Parameter) -> (TypeRef, ParamShape) {
        // `content` on a parameter means "this value is a document in that media type", which is
        // one media type's worth of encoding rather than a style. Typed as its schema and treated
        // as a primitive, because what goes in the URL is the encoded text.
        let id = parameter.schema.or_else(|| {
            parameter
                .content
                .as_ref()
                .and_then(|content| content.values().find_map(|entry| entry.schema))
        });
        let Some(id) = id else {
            return (TypeRef::String, ParamShape::Primitive);
        };
        let key = crate::shape::key_of(self.resolved, id);
        let ty = self
            .contracts
            .type_of(&key)
            .cloned()
            .unwrap_or(TypeRef::String);
        // Derived from the *type the extraction decodes into*, not from the classified shape. The
        // two disagreed for orb's `status[]`: a nullable array classifies as a union, which is not
        // `Shape::Array`, so the shape said "primitive" while the rendered type was
        // `Option<Vec<String>>` — and the server, told it was reading a scalar, handed the typed
        // read a string it rejected. One source answering both questions is the fix, not a second
        // case in the old match.
        let shape = param_shape(&ty, self.contracts);
        (ty, shape)
    }
}

/// Which of the three serialization shapes a parameter's *type* has.
///
/// The type, not the classified schema shape, because the two can disagree and the type is the one
/// the extraction decodes into: orb's `status[]` is a nullable array, which classifies as a union
/// rather than as `Shape::Array`, so the old shape-side derivation said "primitive" while the
/// rendered field was `Option<Vec<String>>` — and the server rejected the very requests its own
/// client builds. Wrappers that do not change what the wire carries are looked through; named
/// types are looked *into*, because an alias of an array is an array on the wire.
pub(super) fn param_shape(ty: &TypeRef, contracts: &Contracts) -> ParamShape {
    match ty {
        TypeRef::Option(inner) | TypeRef::Boxed(inner) => param_shape(inner, contracts),
        TypeRef::Vec(_) | TypeRef::Array(..) | TypeRef::Tuple(_) => ParamShape::Array,
        // Serialized as an object; it only ever sits in a multipart body, never a parameter.
        TypeRef::Map(_) | TypeRef::Upload => ParamShape::Object,
        TypeRef::Named(index) => match contracts
            .get(*index)
            .map(crate::contract::TypeContract::kind)
        {
            Some(ContractKind::Struct { .. }) => ParamShape::Object,
            Some(ContractKind::Tuple { .. }) => ParamShape::Array,
            Some(ContractKind::Newtype { inner } | ContractKind::Alias { target: inner }) => {
                param_shape(inner, contracts)
            }
            // String enums and unions serialize as whatever scalar their value is; `None` is an
            // unresolvable index, which the type path renderer also treats as opaque. A *tagged*
            // union's value is always an object, so `Primitive` is questionable there — but it is
            // the ruling untagged object unions already get, and changing both is a behavioral
            // decision, not this refactor's.
            Some(
                ContractKind::StringEnum { .. }
                | ContractKind::Enum { .. }
                | ContractKind::TaggedEnum { .. }
                | ContractKind::CarriedTagEnum { .. },
            )
            | None => ParamShape::Primitive,
        },
        TypeRef::Unit
        | TypeRef::Bool
        | TypeRef::I64
        | TypeRef::U64
        | TypeRef::F64
        | TypeRef::String
        | TypeRef::Format(_)
        | TypeRef::Value => ParamShape::Primitive,
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};

    use serde_json::json;

    use crate::api::Location;
    use crate::api::tests::{model_of, with_paths};
    use crate::contract::TypeRef;
    use crate::diag::Action;

    #[test_util::test]
    fn an_optional_parameter_with_no_defined_serialization_is_dropped_and_the_operation_stays() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "parameters": [
                        {"name": "filter", "in": "query", "style": "deepObject",
                         "schema": {"type": "array", "items": {"type": "string"}}},
                        {"name": "limit", "in": "query", "schema": {"type": "integer"}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        assert_eq!(model.operations().len(), 1);
        let names: Vec<&str> = model.operations()[0]
            .params
            .iter()
            .map(|param| param.wire_name.as_str())
            .collect();
        assert_eq!(names, ["limit"]);
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::QuerySerializationStyle)
        );
    }

    /// An exploded `form` object is defined, writable — and erased: its members travel under
    /// their own names, so no server can read the parameter back. The parameter stays (the
    /// calling side is whole) and the degradation is said out loud; it used to ship silently,
    /// with the handler's field permanently absent.
    #[test_util::test]
    fn an_exploded_form_object_parameter_is_kept_and_its_erasure_is_reported() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/videos": {
                "get": {
                    "operationId": "streamVideo",
                    "parameters": [
                        {"name": "streamOptions", "in": "query",
                         "schema": {"type": "object", "properties": {"bitrate": {"type": "integer"}}}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        // The parameter survives — only the serving side is degraded.
        assert_eq!(model.operations().len(), 1);
        assert_eq!(model.operations()[0].params.len(), 1);
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::QuerySerializationStyle)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("erased"), "{found:#?}");
        assert!(found[0].contains("always absent"), "{found:#?}");
    }

    #[test_util::test]
    fn a_required_parameter_with_no_defined_serialization_takes_the_operation_with_it() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "parameters": [
                        {"name": "filter", "in": "query", "required": true, "style": "deepObject",
                         "schema": {"type": "array", "items": {"type": "string"}}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        assert!(model.operations().is_empty());
    }

    #[test_util::test]
    fn a_parameter_that_wants_a_name_the_params_struct_uses_takes_a_suffix() {
        // The params struct holds the request body in a field called `body`, so a parameter with
        // that name would declare the field twice and not compile. `client` is no such name: it
        // lives on the request struct, which holds no parameter fields — `jellyfin` declares a
        // query parameter called `client` and keeps it.
        let (model, _) = model_of(with_paths(json!({
            "/sessions": {
                "get": {
                    "operationId": "getSessions",
                    "parameters": [
                        {"name": "client", "in": "query", "schema": {"type": "string"}},
                        {"name": "body", "in": "query", "schema": {"type": "string"}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        let params = &model.operations()[0].params;
        let names: Vec<(&str, &str)> = params
            .iter()
            .map(|param| (param.wire_name.as_str(), param.rust_name.as_str()))
            .collect();
        // The wire names are untouched; only the Rust side moves, which is the rule everywhere.
        assert_eq!(names, [("body", "body2"), ("client", "client")]);
    }

    #[test_util::test]
    fn a_path_parameter_the_template_never_names_is_left_out() {
        // Declared `in: path` with no matching template variable: the client has nowhere to
        // write it and the extractor nothing to read, so transcribing it — required, per the
        // path rule — would demand a value nobody can send and reject every request server-side.
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "parameters": [{"name": "petId", "in": "path", "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        assert!(model.operations()[0].params.is_empty());
        let report = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.detail().contains("no variable of `/pets`"))
            .ok_or_eyre("the drop is reported")?;
        assert_eq!(report.action(), Action::Degrade);
    }

    #[test_util::test]
    fn a_path_parameter_is_required_even_when_the_document_omits_the_flag() {
        // OpenAPI obliges a path parameter to say `required: true`; a document that forgets still
        // described a URL with a hole in it, and the record carries what the URL says.
        let (model, _) = model_of(with_paths(json!({
            "/pets/{petId}": {
                "get": {
                    "operationId": "showPetById",
                    "parameters": [{"name": "petId", "in": "path", "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        let param = &model.operations()[0].params[0];
        assert_eq!(param.wire_name, "petId");
        assert!(param.required);
    }

    #[test_util::test]
    fn an_operation_level_parameter_overrides_the_path_level_one_it_repeats() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "parameters": [{"name": "limit", "in": "query", "schema": {"type": "string"}}],
                "get": {
                    "operationId": "listPets",
                    "parameters": [{"name": "limit", "in": "query", "required": true, "schema": {"type": "integer"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        let params = &model.operations()[0].params;
        assert_eq!(params.len(), 1);
        assert!(params[0].required);
        assert_eq!(params[0].ty, TypeRef::I64);
        assert_eq!(params[0].style.location(), Location::Query);
    }
}
