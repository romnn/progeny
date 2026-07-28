//! Examples that contradict the schema they illustrate.
//!
//! Examples never gate generation — an `example` member is documentation, and refusing to generate
//! a client because a vendor's sample payload is wrong would be the tail wagging the dog. But they
//! are checked, for one reason: the payload round-trip harness deserializes them, and without this
//! it could not tell "progeny generated the wrong type" from "the document's own example does not
//! match the document's own schema". 19 corpus documents carry examples that contradict
//! themselves, so a harness with no verdict for them is a harness that reports 19 false failures.
//!
//! The check is against the **shape**, not against a JSON Schema validator — the rule itself is
//! [`crate::shape::Fit`], shared with the contract layer's default validation so the two verdicts
//! about one JSON value cannot disagree. This module owns the walk over the API surface and the
//! diagnostic, which are the halves the shared rule cannot know.

use serde_json::Value;

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Document, MaybeRef, MediaType, Operation, PathItem};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::{Fit, Shape, Shapes};

/// Whether a value is one this shape describes.
///
/// The payload gate's question when it has to say which branch of a union a payload is: serde tries
/// them in declaration order and takes the first that deserializes, so the harness asks the same
/// question in the same order.
pub(super) fn accepts(shapes: &Shapes, value: &Value, shape: &Shape) -> bool {
    fit(shapes).mismatch(value, shape).is_none()
}

/// The shared rule, speaking this module's language.
fn fit(shapes: &Shapes) -> Fit<'_> {
    Fit::new(shapes, "the example")
}

/// Whether one example contradicts the schema beside it, and how.
///
/// The same question [`report`] asks, exposed because the payload gate needs it *per example*
/// rather than per document: the class aggregates and caps its related locations at five, so a
/// verdict read back out of the diagnostics would be right about the first few examples of a
/// document and silently wrong about the rest — `cloudflare` writes 29.
pub(super) fn contradiction(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    id: SchemaId,
    value: &Value,
) -> Option<String> {
    let key = crate::shape::key_of(resolved, id);
    fit(shapes).mismatch(value, shapes.get(&key)?)
}

/// Check every example the API surface carries against the schema beside it.
pub(super) fn report(resolved: &ResolvedDocument, shapes: &Shapes, ctx: &mut Ctx) {
    let check = Check { resolved, shapes };
    check.document(resolved.document(), ctx);
}

struct Check<'a> {
    resolved: &'a ResolvedDocument,
    shapes: &'a Shapes,
}

impl Check<'_> {
    fn document(&self, document: &Document, ctx: &mut Ctx) {
        let at = JsonPointer::root().child("paths");
        for (route, item) in document
            .paths
            .as_ref()
            .map(|paths| &paths.items)
            .into_iter()
            .flatten()
        {
            let Some(item) = self.resolved.path_item(item) else {
                continue;
            };
            self.path_item(item, &at.child(route.clone()), ctx);
        }
    }

    fn path_item(&self, item: &PathItem, at: &JsonPointer, ctx: &mut Ctx) {
        for (method, operation) in item.operations() {
            self.operation(operation, &at.child(method.slug()), ctx);
        }
    }

    fn operation(&self, operation: &Operation, at: &JsonPointer, ctx: &mut Ctx) {
        if let Some(node) = &operation.request_body
            && let Some(body) = self.resolved.request_body(node)
        {
            self.content(body.content.as_ref(), &at.child("requestBody"), ctx);
        }
        let Some(responses) = &operation.responses else {
            return;
        };
        let at = at.child("responses");
        for (status, node) in &responses.statuses {
            if let Some(response) = self.resolved.response(node) {
                self.content(response.content.as_ref(), &at.child(status.clone()), ctx);
            }
        }
    }

    fn content(
        &self,
        content: Option<&std::collections::BTreeMap<String, MediaType>>,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) {
        for (media_type, entry) in content.into_iter().flatten() {
            let Some(id) = entry.schema else {
                continue;
            };
            let at = at.child("content").child(media_type.clone());
            if let Some(example) = &entry.example {
                self.check(example, id, &at.child("example"), ctx);
            }
            for (name, node) in entry.examples.iter().flatten() {
                let MaybeRef::Item(example) = node else {
                    continue;
                };
                if let Some(value) = &example.value {
                    self.check(value, id, &at.child("examples").child(name.clone()), ctx);
                }
            }
        }
    }

    fn check(&self, example: &Value, id: SchemaId, at: &JsonPointer, ctx: &mut Ctx) {
        let key = crate::shape::key_of(self.resolved, id);
        let Some(shape) = self.shapes.get(&key) else {
            return;
        };
        if let Some(reason) = fit(self.shapes).mismatch(example, shape) {
            ctx.report(Diagnostic::new(
                BreakageClass::InvalidExample,
                Action::Warn,
                at.clone(),
                format!(
                    "the example contradicts the schema it illustrates: {reason}. Generation is \
                     unaffected — an example is documentation — but a payload test built from this \
                     one is testing the document, not the generated code"
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::api::tests::{model_of, with_paths};

    fn response_with(schema: serde_json::Value, example: serde_json::Value) -> serde_json::Value {
        let mut media = serde_json::Map::new();
        media.insert("schema".to_owned(), schema);
        media.insert("example".to_owned(), example);
        let mut content = serde_json::Map::new();
        content.insert(
            "application/json".to_owned(),
            serde_json::Value::Object(media),
        );
        let mut response = serde_json::Map::new();
        response.insert("description".to_owned(), json!("ok"));
        response.insert("content".to_owned(), serde_json::Value::Object(content));
        let mut responses = serde_json::Map::new();
        responses.insert("200".to_owned(), serde_json::Value::Object(response));
        let mut operation = serde_json::Map::new();
        operation.insert("operationId".to_owned(), json!("listPets"));
        operation.insert("responses".to_owned(), serde_json::Value::Object(responses));
        let mut item = serde_json::Map::new();
        item.insert("get".to_owned(), serde_json::Value::Object(operation));
        let mut paths = serde_json::Map::new();
        paths.insert("/pets".to_owned(), serde_json::Value::Object(item));
        with_paths(serde_json::Value::Object(paths))
    }

    fn complaint(schema: serde_json::Value, example: serde_json::Value) -> Option<String> {
        let (_, diagnostics) = model_of(response_with(schema, example));
        diagnostics
            .iter()
            .find(|found| found.class() == crate::BreakageClass::InvalidExample)
            .map(|found| found.detail().to_owned())
    }

    #[test]
    fn an_example_of_the_wrong_type_is_reported() {
        let found = complaint(json!({"type": "string"}), json!(7)).expect("should be reported");
        assert!(found.contains("says a string"), "{found}");
        assert!(found.contains("is a number"), "{found}");
    }

    #[test]
    fn an_example_missing_a_required_property_is_reported() {
        let found = complaint(
            json!({"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}),
            json!({"other": "thing"}),
        )
        .expect("should be reported");
        assert!(found.contains("leaves out `name`"), "{found}");
    }

    #[test]
    fn an_example_outside_an_enum_is_reported() {
        let found = complaint(
            json!({"type": "string", "enum": ["red", "green"]}),
            json!("blue"),
        )
        .expect("should be reported");
        assert!(found.contains("`blue`"), "{found}");

        // And an example that is not a string at all. The enum reads from a string and nothing
        // else, so saying nothing here would hand the payload gate an example it cannot use.
        let found = complaint(
            json!({"type": "string", "enum": ["red", "green"]}),
            json!(7),
        )
        .expect("should be reported");
        assert!(found.contains("says a string"), "{found}");
        assert!(found.contains("is a number"), "{found}");
    }

    #[test]
    fn a_contradiction_inside_a_member_is_still_a_contradiction() {
        // `cloudflare` writes `false` where its own schema says a string, one level down. A check
        // that only reads the top level calls the example sound, and the payload gate then reports
        // the vendor's defect as progeny's.
        let found = complaint(
            json!({
                "type": "object",
                "properties": {"inner": {"type": "object", "properties": {"name": {"type": "string"}}}},
            }),
            json!({"inner": {"name": false}}),
        )
        .expect("should be reported");
        assert!(found.contains("at `inner`"), "{found}");
        assert!(found.contains("at `name`"), "{found}");
        assert!(found.contains("says a string"), "{found}");
    }

    /// The three payloads `github` could not deserialize, each reduced to its schema and example.
    ///
    /// All three are contradictions one element deep, and all three were invisible while the check
    /// stopped at the container: the payload gate reported the vendor's defect as progeny's.
    mod inside_a_container {
        use super::{complaint, json};

        #[test]
        fn an_element_of_the_wrong_type_is_reported() {
            // `/user/codespaces/secrets/{secret_name}/repositories`: the schema says integers and
            // the example writes the same numbers as strings.
            let found = complaint(
                json!({"type": "array", "items": {"type": "integer"}}),
                json!(["1296269", "1296280"]),
            )
            .expect("should be reported");
            assert!(found.contains("at `0`"), "{found}");
            assert!(found.contains("says an integer"), "{found}");
        }

        #[test]
        fn an_element_missing_a_required_member_is_reported() {
            // `/orgs/{org}/issue-fields`: the option schema requires `priority` and none of the
            // three options the example lists carries it.
            let found = complaint(
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "priority"],
                        "properties": {"name": {"type": "string"}, "priority": {"type": "integer"}},
                    },
                }),
                json!([{"name": "High"}]),
            )
            .expect("should be reported");
            assert!(found.contains("at `0`"), "{found}");
            assert!(found.contains("leaves out `priority`"), "{found}");
        }

        #[test]
        fn an_element_no_branch_of_a_union_accepts_is_reported() {
            // `/orgs/{org}/copilot-spaces/{space_number}/collaborators`: the team collaborator in
            // the example omits `type`, which the team branch requires, and its `actor_type` rules
            // out the user branch — so nothing accepts it.
            let found = complaint(
                json!({
                    "type": "array",
                    "items": {"anyOf": [
                        {
                            "type": "object",
                            "required": ["actor_type", "login"],
                            "properties": {
                                "actor_type": {"type": "string", "enum": ["User"]},
                                "login": {"type": "string"},
                            },
                        },
                        {
                            "type": "object",
                            "required": ["actor_type", "type"],
                            "properties": {
                                "actor_type": {"type": "string", "enum": ["Team"]},
                                "type": {"type": "string", "enum": ["Team"]},
                            },
                        },
                    ]},
                }),
                json!([{"actor_type": "Team", "name": "Developers"}]),
            )
            .expect("should be reported");
            assert!(found.contains("at `0`"), "{found}");
            assert!(found.contains("no branch"), "{found}");
        }

        #[test]
        fn a_map_value_that_contradicts_the_schema_is_reported() {
            let found = complaint(
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                json!({"one": 1}),
            )
            .expect("should be reported");
            assert!(found.contains("at `one`"), "{found}");
            assert!(found.contains("says a string"), "{found}");
        }

        #[test]
        fn a_tuple_the_example_gives_the_wrong_length_is_reported() {
            // A tuple lowers to a Rust tuple, which serde reads only at exactly its length — so
            // this fails to deserialize whether or not the document allows the third element.
            let found = complaint(
                json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}]}),
                json!(["a", 1, true]),
            )
            .expect("should be reported");
            assert!(found.contains("says 2 elements"), "{found}");
        }
    }

    /// The disagreement the two old checkers allowed, pinned shut.
    ///
    /// One JSON literal, written as both a property's `default` and the payload's `example`:
    /// the shallow `TypeRef` checker called the default sound (any object fit a named struct)
    /// while the recursive shape checker called the example wrong, so a reader was told about
    /// half the defect. Both verdicts now come from [`crate::shape::Fit`], so both records
    /// appear — same reason, each in its own vocabulary.
    #[test]
    fn a_wrong_literal_used_as_default_and_example_gets_both_verdicts() {
        let (_, diagnostics) = model_of(response_with(
            json!({
                "type": "object",
                "properties": {
                    "config": {
                        "type": "object",
                        "required": ["name"],
                        "properties": {"name": {"type": "string"}},
                        "default": {"name": false},
                    },
                },
            }),
            json!({"config": {"name": false}}),
        ));
        let detail_of = |class: crate::BreakageClass| {
            diagnostics
                .iter()
                .find(|found| found.class() == class)
                .map(|found| found.detail().to_owned())
                .unwrap_or_else(|| panic!("no `{class}` record"))
        };
        let example = detail_of(crate::BreakageClass::InvalidExample);
        assert!(example.contains("at `name`"), "{example}");
        assert!(example.contains("the example is a boolean"), "{example}");
        let default = detail_of(crate::BreakageClass::InvalidDefault);
        assert!(default.contains("at `name`"), "{default}");
        assert!(default.contains("the default is a boolean"), "{default}");
    }

    #[test]
    fn an_example_that_agrees_with_its_schema_says_nothing() {
        assert_eq!(complaint(json!({"type": "string"}), json!("hello")), None);
        assert_eq!(
            complaint(
                json!({"type": "object", "properties": {"name": {"type": "string"}}}),
                json!({"name": "Rex", "extra": 1}),
            ),
            None
        );
        // An integer written as a whole float is JSON having one number type, not a contradiction.
        assert_eq!(complaint(json!({"type": "integer"}), json!(1.0)), None);
        // A nullable schema and a null example.
        assert_eq!(
            complaint(json!({"type": ["string", "null"]}), json!(null)),
            None
        );
        // Elements that agree, and a list the document left untyped — which constrains nothing, so
        // recursing into it must stay silent rather than guess.
        assert_eq!(
            complaint(
                json!({"type": "array", "items": {"type": "integer"}}),
                json!([1, 2]),
            ),
            None
        );
        assert_eq!(
            complaint(json!({"type": "array"}), json!([1, "two", null])),
            None
        );
    }

    #[test]
    fn a_union_is_only_wrong_when_no_branch_accepts_the_example() {
        assert_eq!(
            complaint(
                json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}),
                json!(7),
            ),
            None
        );
        let found = complaint(
            json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}),
            json!(true),
        )
        .expect("should be reported");
        assert!(found.contains("no branch"), "{found}");
    }
}
