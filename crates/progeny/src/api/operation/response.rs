//! The status arms: parsed from the response keys, typed, and put in overlap order.

use std::collections::BTreeSet;

use super::Build;
use super::body::is_json;
use crate::api::{
    HeaderTypes, MODULE_TYPES, OperationContract, ResponseArm, ResponseBody, ResponseContract,
    StatusPattern, type_suffix,
};
use crate::contract::{Namer, RustIdent, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MaybeRef, Operation, Response};
use crate::shape::Docs;

impl Build<'_> {
    /// The status arms, in the order overlap resolution puts them.
    pub(super) fn responses(
        &mut self,
        operation: &Operation,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> ResponseContract {
        let Some(responses) = &operation.responses else {
            return ResponseContract::default();
        };
        let at = at.child("responses");
        let mut used = Namer::default();
        let mut accept = Vec::new();
        let mut arms = Vec::new();
        for (status, node) in &responses.statuses {
            let Some(pattern) = parse_status(status) else {
                ctx.report(Diagnostic::new(
                    BreakageClass::MalformedMember,
                    Action::Degrade,
                    at.child(status.clone()),
                    format!(
                        "`{status}` is neither a status code nor a range, so no arm is generated \
                         for it"
                    ),
                ));
                continue;
            };
            if pattern == StatusPattern::Exact(101) {
                ctx.report(Diagnostic::new(
                    BreakageClass::ConnectionUpgrade,
                    Action::Degrade,
                    at.child(status.clone()),
                    "the response switches protocols, but the generated client does not return \
                     the upgraded connection and the generated server cannot serve it; only the \
                     status arm is generated",
                ));
            }
            arms.push(self.arm(
                pattern,
                node,
                &at.child(status.clone()),
                &mut used,
                &mut accept,
                ctx,
            ));
        }
        arms.sort_by_key(|arm| arm.status.precedence());

        let default = responses.default.as_ref().map(|node| {
            // The default arm claims whatever no other arm does, so it has no pattern of its own;
            // `Range(0)` is not a status any response can have, which is what makes it usable as
            // the sentinel here without colliding with a real arm.
            self.arm(
                StatusPattern::Range(0),
                node,
                &at.child("default"),
                &mut used,
                &mut accept,
                ctx,
            )
        });
        // A configured pick is only worth saying on the wire when it says the *whole* truth:
        // `Accept: application/problem+json` alone, picked for the 404 arm, would tell a
        // strictly negotiating server that the client cannot read the 200's `application/json`
        // — a 406 on the success path the configuration never mentioned. So once any pick makes
        // the header worth sending, every arm's chosen media type joins it, configured picks
        // first.
        if !accept.is_empty() {
            for arm in arms.iter().chain(default.as_ref()) {
                let chosen = match &arm.body {
                    ResponseBody::Json { content_type, .. }
                    | ResponseBody::Text { content_type }
                    | ResponseBody::Bytes { content_type } => content_type,
                    ResponseBody::Empty => continue,
                };
                if !accept.contains(chosen) {
                    accept.push(chosen.clone());
                }
            }
        }
        ResponseContract {
            arms,
            default,
            accept,
        }
    }

    fn arm(
        &mut self,
        status: StatusPattern,
        node: &MaybeRef<Response>,
        at: &JsonPointer,
        used: &mut Namer,
        accept: &mut Vec<String>,
        ctx: &mut Ctx,
    ) -> ResponseArm {
        let name = RustIdent::variant(&status_name(status));
        let Some(response) = self.resolved.response(node) else {
            // The decision `miro` forces, made rather than discovered: a dangling *document-level*
            // reference degrades the position it is at, never the operation around it. 612 of these
            // in one document, and skipping an operation per reference would delete the API.
            ctx.report(Diagnostic::new(
                BreakageClass::DanglingRef,
                Action::Degrade,
                at.clone(),
                "the response references a component the document does not declare; the arm is \
                 generated with its body typed as arbitrary JSON rather than dropped",
            ));
            return ResponseArm {
                status,
                body: ResponseBody::Json {
                    ty: TypeRef::Value,
                    content_type: "application/json".to_owned(),
                },
                headers: Vec::new(),
                header_types: None,
                docs: Docs::default(),
                rust_name: used.unique(name),
                origin: at.clone(),
            };
        };

        let body = match response.content.as_ref() {
            // A response with no content is a status and nothing else: 204, and every arm that
            // says only what went wrong.
            None => ResponseBody::Empty,
            Some(content) => match self.select(content, at, accept, ctx) {
                None => ResponseBody::Empty,
                // A response decodes into the read form of its type: the same type where the
                // strict and lenient shapes agree, the `read::` twin where they do not, and the
                // type itself under strict decoding.
                Some((media_type, entry)) if is_json(&media_type_stem(media_type)) => {
                    ResponseBody::Json {
                        ty: self
                            .contracts
                            .read_form(&entry.schema.map_or(TypeRef::Value, |id| self.type_at(id))),
                        content_type: media_type.to_owned(),
                    }
                }
                Some((media_type, _)) if media_type_stem(media_type).starts_with("text/") => {
                    ResponseBody::Text {
                        content_type: media_type.to_owned(),
                    }
                }
                Some((media_type, _)) => ResponseBody::Bytes {
                    content_type: media_type.to_owned(),
                },
            },
        };
        ResponseArm {
            status,
            body,
            headers: self.headers(response, at, ctx),
            // Named once every operation is, by `name_header_types`.
            header_types: None,
            docs: Docs {
                description: response.description.clone(),
                ..Docs::default()
            },
            rust_name: used.unique(name),
            origin: at.clone(),
        }
    }
}

/// Names the payload and the header group of every arm that types a header, once every
/// operation has its stem.
///
/// `{Operation}{Arm}` can be another operation's type: `foo`'s `400` is `FooBadRequest`, which is
/// also `fooBad`'s request builder, and `foo`'s `500` is `FooInternalServerError`, which is
/// `fooInternalServer`'s enum of failures.
/// So every name a renderer builds from a stem and one of the [`type_suffix::ALL`] tails is
/// claimed first, for every operation, whether it is generated or not, beside the
/// [`MODULE_TYPES`] the operations' modules already hold.
/// Then each arm in model order takes the first of `FooBadRequest`, `FooBadRequest2`, … whose
/// payload and group are both free, so the two always share a spelling.
pub(super) fn name_header_types(operations: &mut [OperationContract]) {
    let mut taken: BTreeSet<String> = operations
        .iter()
        .flat_map(|operation| {
            let stem = operation.rust_name.type_stem();
            type_suffix::ALL.map(|suffix| RustIdent::stem_type(&stem, suffix).to_string())
        })
        .chain(MODULE_TYPES.map(str::to_owned))
        .collect();
    for operation in operations.iter_mut() {
        let stem = operation.rust_name.type_stem();
        let responses = &mut operation.responses;
        for arm in responses.arms.iter_mut().chain(responses.default.as_mut()) {
            if arm.headers.is_empty() {
                arm.header_types = None;
                continue;
            }
            for index in 1u32.. {
                let tail = if index == 1 {
                    arm.rust_name.to_string()
                } else {
                    format!("{}{index}", arm.rust_name)
                };
                let payload = RustIdent::stem_type(&stem, &tail);
                let group = RustIdent::stem_type(&stem, &format!("{tail}{}", type_suffix::HEADERS));
                if !taken.contains(payload.as_str()) && !taken.contains(group.as_str()) {
                    taken.insert(payload.to_string());
                    taken.insert(group.to_string());
                    arm.header_types = Some(HeaderTypes { payload, group });
                    break;
                }
            }
        }
    }
}

fn media_type_stem(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase()
}

/// A status key as a pattern, or nothing when it is neither.
fn parse_status(key: &str) -> Option<StatusPattern> {
    if let Ok(code) = key.parse::<u16>() {
        // Anything outside the range HTTP defines is a typo rather than a status.
        return (100..600)
            .contains(&code)
            .then_some(StatusPattern::Exact(code));
    }
    let mut characters = key.chars();
    let first = characters.next()?.to_digit(10)?;
    let rest: String = characters.collect();
    if !rest.eq_ignore_ascii_case("xx") || !(1..=5).contains(&first) {
        return None;
    }
    u8::try_from(first).ok().map(StatusPattern::Range)
}

/// The variant name an arm contributes.
fn status_name(status: StatusPattern) -> String {
    match status {
        StatusPattern::Exact(code) => match code {
            200 => "Ok".to_owned(),
            201 => "Created".to_owned(),
            202 => "Accepted".to_owned(),
            204 => "NoContent".to_owned(),
            400 => "BadRequest".to_owned(),
            401 => "Unauthorized".to_owned(),
            403 => "Forbidden".to_owned(),
            404 => "NotFound".to_owned(),
            409 => "Conflict".to_owned(),
            422 => "UnprocessableEntity".to_owned(),
            429 => "TooManyRequests".to_owned(),
            500 => "InternalServerError".to_owned(),
            other => format!("Status{other}"),
        },
        StatusPattern::Range(0) => "Default".to_owned(),
        StatusPattern::Range(hundreds) => format!("Status{hundreds}xx"),
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};

    use serde_json::json;

    use super::parse_status;
    use crate::api::tests::{model_of, with_paths};
    use crate::api::{ResponseBody, StatusPattern};
    use crate::contract::{RustIdent, TypeRef};

    #[test_util::test]
    fn a_status_key_is_a_code_or_a_range_and_nothing_else() {
        assert_eq!(parse_status("200"), Some(StatusPattern::Exact(200)));
        assert_eq!(parse_status("2XX"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("2xx"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("999"), None);
        assert_eq!(parse_status("0XX"), None);
        assert_eq!(parse_status("x-vendor"), None);
        assert_eq!(parse_status(""), None);
    }

    #[test_util::test]
    fn a_configured_media_type_applies_to_a_response_position() -> eyre::Result<()> {
        // A preview endpoint declaring the image beside its JSON metadata: the preference picks
        // JSON, and a caller who wants the picture says so at the response's own pointer.
        let document = with_paths(json!({
            "/preview": {
                "get": {
                    "operationId": "getPreview",
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": {"schema": {"type": "object", "properties": {"note": {"type": "string"}}}},
                                "image/jpeg": {"schema": {"type": "string", "format": "binary"}},
                            },
                        },
                    },
                },
            },
        }));
        let config = crate::config::Config {
            media_types: [(
                "/paths/~1preview/get/responses/200/content".to_owned(),
                "image/jpeg".to_owned(),
            )]
            .into_iter()
            .collect(),
            ..crate::config::Config::default()
        };
        let (model, _) = crate::api::tests::model_with(document, &config)?;
        // The pick also rides the request: a choice the server never hears is not one.
        assert_eq!(
            model.operations()[0].responses.accept,
            vec!["image/jpeg".to_owned()]
        );
        assert_eq!(
            model.operations()[0].responses.arms[0].body,
            ResponseBody::Bytes {
                content_type: "image/jpeg".to_owned()
            }
        );
        Ok(())
    }

    #[test_util::test]
    fn a_pick_on_an_error_arm_still_accepts_the_success_type() -> eyre::Result<()> {
        // Configuring the 404's decode is a natural thing to want when only the error arm is
        // ambiguous. The header it earns must still say the client reads the 200's JSON, or a
        // strictly negotiating server answers 406 on the success path the configuration never
        // mentioned.
        let document = with_paths(json!({
            "/x": {
                "get": {
                    "operationId": "getX",
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {"application/json": {"schema": {"type": "boolean"}}},
                        },
                        "404": {
                            "description": "missing",
                            "content": {
                                "application/json": {"schema": {"type": "object"}},
                                "application/problem+json": {"schema": {"type": "object"}},
                            },
                        },
                    },
                },
            },
        }));
        let config = crate::config::Config {
            media_types: [(
                "/paths/~1x/get/responses/404/content".to_owned(),
                "application/problem+json".to_owned(),
            )]
            .into_iter()
            .collect(),
            ..crate::config::Config::default()
        };
        let (model, _) = crate::api::tests::model_with(document, &config)?;
        assert_eq!(
            model.operations()[0].responses.accept,
            vec![
                "application/problem+json".to_owned(),
                "application/json".to_owned(),
            ]
        );
        Ok(())
    }

    #[test_util::test]
    fn a_response_referencing_a_component_that_does_not_exist_degrades_its_arm_not_its_operation() {
        // `miro`, in miniature: a reference to a `components.responses` section the document never
        // declares.
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": {
                        "200": {"$ref": "#/components/responses/200"},
                        "404": {"description": "gone"},
                    },
                },
            },
        })))?;
        assert_eq!(model.operations().len(), 1);
        let responses = &model.operations()[0].responses;
        assert_eq!(responses.arms.len(), 2);
        assert_eq!(responses.arms[0].status, StatusPattern::Exact(200));
        assert_eq!(
            responses.arms[0].body,
            ResponseBody::Json {
                ty: TypeRef::Value,
                content_type: "application/json".to_owned(),
            }
        );
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::DanglingRef)
        );
    }

    #[test_util::test]
    fn a_switching_protocols_arm_names_the_connection_upgrade_progeny_does_not_perform() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/socket": {
                "get": {
                    "operationId": "connect",
                    "responses": {
                        "101": {"description": "switching protocols"},
                    },
                },
            },
        })))?;

        assert_eq!(
            model.operations()[0].responses.arms[0].status,
            StatusPattern::Exact(101)
        );
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::ConnectionUpgrade)
        );
    }

    #[test_util::test]
    fn exact_arms_come_before_the_ranges_they_overlap() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": {
                        "5XX": {"description": "broken"},
                        "200": {"description": "ok"},
                        "2XX": {"description": "fine"},
                        "404": {"description": "gone"},
                        "default": {"description": "anything else"},
                    },
                },
            },
        })))?;
        let responses = &model.operations()[0].responses;
        let patterns: Vec<StatusPattern> = responses.arms.iter().map(|arm| arm.status).collect();
        assert_eq!(
            patterns,
            [
                StatusPattern::Exact(200),
                StatusPattern::Exact(404),
                StatusPattern::Range(2),
                StatusPattern::Range(5),
            ]
        );
        assert!(responses.default.is_some());
        assert_eq!(responses.arms[0].rust_name.as_str(), "Ok");
        assert_eq!(responses.arms[2].rust_name.as_str(), "Status2xx");
    }

    #[test_util::test]
    fn each_response_arm_carries_the_wire_kind_selected_for_it() {
        let (model, _) = model_of(with_paths(json!({
            "/payloads": {
                "get": {
                    "operationId": "payloads",
                    "responses": {
                        "200": {"description": "json", "content": {
                            "application/problem+json; charset=utf-8": {
                                "schema": {"type": "boolean"}
                            }
                        }},
                        "201": {"description": "text", "content": {
                            "TEXT/PLAIN; charset=utf-8": {"schema": {"type": "string"}}
                        }},
                        "202": {"description": "bytes", "content": {
                            "application/octet-stream": {
                                "schema": {
                                    "type": "string",
                                    "contentMediaType": "application/octet-stream"
                                }
                            }
                        }},
                        "204": {"description": "empty"},
                    },
                },
            },
        })))?;
        let arms = &model.operations()[0].responses.arms;
        // The exact declared spelling, parameters included: it is what the generated server
        // writes as the response's `Content-Type`.
        assert_eq!(
            arms[0].body,
            ResponseBody::Json {
                ty: TypeRef::Bool,
                content_type: "application/problem+json; charset=utf-8".to_owned(),
            }
        );
        assert_eq!(
            arms[1].body,
            ResponseBody::Text {
                content_type: "TEXT/PLAIN; charset=utf-8".to_owned(),
            }
        );
        assert_eq!(
            arms[2].body,
            ResponseBody::Bytes {
                content_type: "application/octet-stream".to_owned(),
            }
        );
        assert_eq!(arms[3].body, ResponseBody::Empty);
    }

    /// Every arm's payload and header group, as `Operation.Arm: Payload / Group`, in model order;
    /// an arm that types no header has neither.
    fn header_types(model: &crate::api::ApiModel) -> Vec<String> {
        model
            .operations()
            .iter()
            .flat_map(|operation| {
                let responses = &operation.responses;
                responses
                    .arms
                    .iter()
                    .chain(&responses.default)
                    .map(move |arm| match &arm.header_types {
                        Some(types) => format!(
                            "{}.{}: {} / {}",
                            operation.rust_name, arm.rust_name, types.payload, types.group
                        ),
                        None => format!("{}.{}: none", operation.rust_name, arm.rust_name),
                    })
            })
            .collect()
    }

    /// An arm's payload and group take a suffix where `{Operation}{Arm}` is a type another
    /// operation generates, and keep their plain names otherwise.
    ///
    /// `foo`'s `400` would be `FooBadRequest`, the request builder of `fooBad`, and its `500`
    /// `FooInternalServerError`, the enum of failures `fooInternalServer` would have with a
    /// second error: every such name is claimed whether the renderers emit it or not, so a
    /// description gaining an error arm does not rename another operation's payload.
    #[test_util::test]
    fn an_arm_type_another_operation_generates_takes_a_suffix() {
        let typed = json!({"X-Reason": {"schema": {"type": "string"}}});
        let (model, _) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "foo", "responses": {
                "400": {"description": "refused", "headers": typed},
                "404": {"description": "missing", "headers": typed},
                "500": {"description": "failed", "headers": typed},
            }}},
            "/b": {"get": {"operationId": "fooBad", "responses": {
                "204": {"description": "done"},
            }}},
            "/c": {"get": {"operationId": "fooInternalServer", "responses": {
                "204": {"description": "done", "headers": typed},
            }}},
        })))?;
        assert_eq!(
            header_types(&model),
            [
                "foo.BadRequest: FooBadRequest2 / FooBadRequest2Headers",
                "foo.NotFound: FooNotFound / FooNotFoundHeaders",
                "foo.InternalServerError: FooInternalServerError2 / FooInternalServerError2Headers",
                "foo_bad.NoContent: none",
                "foo_internal_server.NoContent: FooInternalServerNoContent / FooInternalServerNoContentHeaders",
            ]
        );
    }

    /// Two arms whose `{Operation}{Arm}` meet are told apart in model order, the payload and its
    /// group together.
    ///
    /// No two arm names the model gives today end alike, so the arms here are renamed the way an
    /// arm named after a status phrase would be: `fooNot`'s `Found` meets `foo`'s `NotFound`.
    #[test_util::test]
    fn two_arms_whose_types_meet_are_told_apart_in_model_order() {
        let typed = json!({"X-Reason": {"schema": {"type": "string"}}});
        let (mut model, _) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "foo", "responses": {
                "404": {"description": "missing", "headers": typed},
            }}},
            "/b": {"get": {"operationId": "fooNot", "responses": {
                "302": {"description": "elsewhere", "headers": typed},
            }}},
        })))?;
        let found = model
            .operations
            .get_mut(1)
            .and_then(|operation| operation.responses.arms.first_mut())
            .ok_or_eyre("`fooNot`'s arm")?;
        found.rust_name = RustIdent::variant("Found");
        super::name_header_types(&mut model.operations);
        assert_eq!(
            header_types(&model),
            [
                "foo.NotFound: FooNotFound / FooNotFoundHeaders",
                "foo_not.Found: FooNotFound2 / FooNotFound2Headers",
            ]
        );
    }
}
