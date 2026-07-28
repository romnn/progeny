//! The status arms: parsed from the response keys, typed, and put in overlap order.

use super::Build;
use super::body::is_json;
use crate::api::{ResponseArm, ResponseContract, StatusPattern};
use crate::contract::{Namer, RustIdent, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MaybeRef, Operation, Response};
use crate::shape::Docs;

impl Build<'_> {
    /// The status arms, in the order overlap resolution puts them.
    pub(super) fn responses(
        &self,
        operation: &Operation,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> ResponseContract {
        let Some(responses) = &operation.responses else {
            return ResponseContract::default();
        };
        let at = at.child("responses");
        let mut used = Namer::default();
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
            arms.push(self.arm(pattern, node, &at.child(status.clone()), &mut used, ctx));
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
                ctx,
            )
        });
        ResponseContract { arms, default }
    }

    fn arm(
        &self,
        status: StatusPattern,
        node: &MaybeRef<Response>,
        at: &JsonPointer,
        used: &mut Namer,
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
                ty: TypeRef::Value,
                docs: Docs::default(),
                rust_name: used.unique(name),
            };
        };

        let ty = match response.content.as_ref() {
            // A response with no content is a status and nothing else: 204, and every arm that
            // says only what went wrong.
            None => TypeRef::Unit,
            Some(content) => match Self::select(content, at, ctx) {
                None => TypeRef::Unit,
                Some((media_type, entry)) if is_json(media_type) => entry
                    .schema
                    .map_or(TypeRef::Value, |id| self.type_at(id))
                    .clone(),
                // A body progeny does not type yet arrives as bytes, which is what it is.
                Some(_) => TypeRef::Vec(Box::new(TypeRef::U64)),
            },
        };
        ResponseArm {
            status,
            ty,
            docs: Docs {
                description: response.description.clone(),
                ..Docs::default()
            },
            rust_name: used.unique(name),
        }
    }
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
    use serde_json::json;

    use super::parse_status;
    use crate::api::StatusPattern;
    use crate::api::tests::{model_of, with_paths};
    use crate::contract::TypeRef;

    #[test]
    fn a_status_key_is_a_code_or_a_range_and_nothing_else() {
        assert_eq!(parse_status("200"), Some(StatusPattern::Exact(200)));
        assert_eq!(parse_status("2XX"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("2xx"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("999"), None);
        assert_eq!(parse_status("0XX"), None);
        assert_eq!(parse_status("x-vendor"), None);
        assert_eq!(parse_status(""), None);
    }

    #[test]
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
        })));
        assert_eq!(model.operations().len(), 1);
        let responses = &model.operations()[0].responses;
        assert_eq!(responses.arms.len(), 2);
        assert_eq!(responses.arms[0].status, StatusPattern::Exact(200));
        assert_eq!(responses.arms[0].ty, TypeRef::Value);
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::DanglingRef)
        );
    }

    #[test]
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
        })));
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
}
