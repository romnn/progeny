//! Examples that contradict the schema they illustrate.
//!
//! Examples never gate generation — an `example` member is documentation, and refusing to generate
//! a client because a vendor's sample payload is wrong would be the tail wagging the dog. But they
//! are checked, for one reason: the payload round-trip harness deserializes them, and without this
//! it could not tell "progeny generated the wrong type" from "the document's own example does not
//! match the document's own schema". 19 corpus documents carry examples that contradict
//! themselves, so a harness with no verdict for them is a harness that reports 19 false failures.
//!
//! The check is against the **shape**, not against a JSON Schema validator. Constraints progeny
//! does not turn into a type — `minLength`, `pattern`, `multipleOf` — are not checked, because an
//! example violating one still deserializes into the generated type, which is the only property
//! the harness depends on. Checking more here would produce findings that mean nothing downstream.

use serde_json::Value;

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Document, MaybeRef, MediaType, Operation, PathItem};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::{Shape, ShapeRef, Shapes, Struct, Union};

/// Whether a value is one this shape describes.
///
/// The payload gate's question when it has to say which branch of a union a payload is: serde tries
/// them in declaration order and takes the first that deserializes, so the harness asks the same
/// question in the same order.
pub(super) fn accepts(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    value: &Value,
    shape: &Shape,
) -> bool {
    let check = Check { resolved, shapes };
    check.mismatch(value, shape).is_none()
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
    let check = Check { resolved, shapes };
    let key = crate::shape::key_of(resolved, id);
    check.mismatch(value, shapes.get(&key)?)
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
        if let Some(reason) = self.mismatch(example, shape) {
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

    /// Why a value cannot be what a shape describes, if it cannot.
    ///
    /// Deliberately one-sided: it reports only what is certainly wrong. A shape progeny degraded to
    /// `Any` accepts everything, an absent constraint constrains nothing, and both answer "no
    /// mismatch" — because the question this exists to answer is whether the harness may trust the
    /// example, and an uncertain "maybe" is a false failure waiting to happen.
    fn mismatch(&self, value: &Value, shape: &Shape) -> Option<String> {
        match shape {
            Shape::Any => None,
            Shape::Null => (!value.is_null())
                .then(|| format!("the schema says null, the example is {}", kind(value))),
            Shape::Optional(inner) => {
                if value.is_null() {
                    return None;
                }
                self.through(value, inner)
            }
            Shape::Alias(inner) => self.through(value, inner),
            Shape::Scalar(scalar) => scalar_mismatch(value, *scalar),
            // A format is a string with a spelling rule progeny does not check here.
            Shape::Format(_) => (!value.is_string())
                .then(|| format!("the schema says a string, the example is {}", kind(value))),
            // Only reached when every listed value is a string — a mixed `enum` degrades to `Any`
            // long before this — so the generated type is a unit-variant enum that reads from a
            // string and nothing else. A non-string here is as certainly wrong as a wrong spelling.
            Shape::StringEnum(values) => {
                let Some(text) = value.as_str() else {
                    return Some(format!(
                        "the schema says a string, the example is {}",
                        kind(value)
                    ));
                };
                (!values.iter().any(|allowed| allowed == text)).then(|| {
                    format!(
                        "`{text}` is not one of the {} values the enum allows",
                        values.len()
                    )
                })
            }
            Shape::Struct(structure) => self.structure(value, structure),
            Shape::Map { value: element } => self.mapping(value, element.as_ref()),
            // Recursing into elements matters for the same reason it matters into a struct's
            // members, and the containers were the half that was missing: all three payloads
            // `github` could not deserialize are contradictions one element deep — a string where
            // the schema says an integer, an option missing a required member, a union branch
            // nothing accepts. A check that stops at the container calls each example sound, and
            // the payload gate then reports the vendor's defect as progeny's.
            Shape::Array { item } => self.list(value, item.as_ref()),
            Shape::FixedArray { item, len } => self.fixed(value, item, *len),
            Shape::Tuple { items, .. } => self.tuple(value, items),
            Shape::Union(union) => self.union(value, union),
        }
    }

    fn structure(&self, value: &Value, structure: &Struct) -> Option<String> {
        let Value::Object(object) = value else {
            return Some(not_an_object(value));
        };
        for field in &structure.fields {
            let Some(present) = object.get(&field.wire) else {
                if field.required {
                    return Some(format!(
                        "the example leaves out `{}`, which the schema requires",
                        field.wire
                    ));
                }
                continue;
            };
            // Recursing matters more than it looks: a contradiction three members deep is still a
            // contradiction, and the payload gate's verdict for the whole example turns on it.
            // `cloudflare` writes `false` where its own schema says a string, inside a nested
            // object — a top-level check calls that example sound and the gate then reports the
            // vendor's defect as progeny's.
            if let Some(reason) = self.through(present, &field.shape) {
                return Some(format!("at `{}`, {reason}", field.wire));
            }
        }
        None
    }

    fn mapping(&self, value: &Value, element: Option<&ShapeRef>) -> Option<String> {
        let Value::Object(members) = value else {
            return Some(not_an_object(value));
        };
        // A map the document left untyped constrains its values not at all.
        let element = element?;
        for (key, member) in members {
            if let Some(reason) = self.through(member, element) {
                return Some(format!("at `{key}`, {reason}"));
            }
        }
        None
    }

    fn list(&self, value: &Value, item: Option<&ShapeRef>) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(not_an_array(value));
        };
        // A list the document left untyped constrains its elements not at all.
        self.uniform(items, item?)
    }

    fn fixed(&self, value: &Value, item: &ShapeRef, len: u32) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(not_an_array(value));
        };
        if items.len() != len as usize {
            return Some(format!(
                "the schema says {len} elements, the example has {}",
                items.len()
            ));
        }
        self.uniform(items, item)
    }

    /// `rest` is deliberately not consulted: a tuple lowers to a Rust tuple whether or not the
    /// document allows elements past the prefix, and serde reads one only from an array of exactly
    /// that length — so a longer example fails to deserialize even where the schema permits it.
    fn tuple(&self, value: &Value, positions: &[ShapeRef]) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(not_an_array(value));
        };
        if items.len() != positions.len() {
            return Some(format!(
                "the schema says {} elements, the example has {}",
                positions.len(),
                items.len()
            ));
        }
        for (index, (element, position)) in items.iter().zip(positions).enumerate() {
            if let Some(reason) = self.through(element, position) {
                return Some(format!("at `{index}`, {reason}"));
            }
        }
        None
    }

    /// A union accepts whatever any branch accepts, and progeny already refuses to emit one whose
    /// branches nothing tells apart — so "no branch accepts this" is the only sound finding, and it
    /// needs every branch checked rather than any one of them.
    fn union(&self, value: &Value, union: &Union) -> Option<String> {
        let mut reasons = Vec::new();
        for variant in &union.variants {
            let reason = self.through(value, &variant.shape)?;
            reasons.push(reason);
        }
        (!reasons.is_empty())
            .then(|| format!("no branch of the union accepts it ({})", reasons.join("; ")))
    }

    /// Why one of these elements is not what the element shape describes, if one is not.
    fn uniform(&self, items: &[Value], item: &ShapeRef) -> Option<String> {
        for (index, element) in items.iter().enumerate() {
            if let Some(reason) = self.through(element, item) {
                return Some(format!("at `{index}`, {reason}"));
            }
        }
        None
    }

    fn through(&self, value: &Value, reference: &ShapeRef) -> Option<String> {
        match reference {
            ShapeRef::Key(key) => self.mismatch(value, self.shapes.get(key)?),
            ShapeRef::Inline(shape) => self.mismatch(value, shape),
        }
    }
}

fn not_an_array(value: &Value) -> String {
    format!("the schema says an array, the example is {}", kind(value))
}

fn not_an_object(value: &Value) -> String {
    format!("the schema says an object, the example is {}", kind(value))
}

fn scalar_mismatch(value: &Value, scalar: crate::shape::Scalar) -> Option<String> {
    use crate::shape::Scalar;
    let ok = match scalar {
        Scalar::Bool => value.is_boolean(),
        // An integer schema with a `1.0` example is the document being loose about JSON's one
        // number type, not a contradiction: it deserializes.
        Scalar::Integer { .. } => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        Scalar::Number => value.is_number(),
        Scalar::String => value.is_string(),
    };
    (!ok).then(|| {
        format!(
            "the schema says {}, the example is {}",
            match scalar {
                Scalar::Bool => "a boolean",
                Scalar::Integer { .. } => "an integer",
                Scalar::Number => "a number",
                Scalar::String => "a string",
            },
            kind(value)
        )
    })
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
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
