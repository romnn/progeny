//! The example payloads a document carries, and what the generated types must do with them.
//!
//! Every gate before this one asks a question about *source*: does it generate, does it compile,
//! does the model hold what the document said. None of them runs serde against data, which is
//! exactly the class of defect stage 4's review found five of — code that generates, compiles,
//! round-trips and snapshots green, and is wrong only about payloads.
//!
//! **The comparison is against the original payload, never against a second round of the type's
//! own output.** A member the type drops uniformly survives an idempotence check forever, because
//! the second round drops it too. So this hands the harness both the payload as written and what
//! the document says a faithful round trip keeps, and the generated test compares against the
//! second.
//!
//! What "the document says it keeps" means, precisely: the payload restricted to the members the
//! *shape* declares. A member the schema never named is not carried by the generated type and its
//! absence afterwards is correct, not a loss. Computing that from the shape rather than from the
//! rendering is what makes the test a test — it is the type layer's claim about what it carries,
//! checked against what the emitted Rust actually carries.

use serde_json::{Map, Value};

use super::{ApiModel, BodyContract};
use crate::contract::{Contracts, TypeRef};
use crate::diag::JsonPointer;
use crate::doc::{MaybeRef, MediaType};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::{Extra, Shape, ShapeRef, Shapes};

/// One example payload, and what a faithful round trip through the generated type must produce.
#[derive(Debug, Clone)]
pub(crate) struct Payload {
    /// Where the example was written.
    pub(crate) location: String,
    /// The generated type the payload deserializes into.
    pub(crate) type_name: String,
    /// The payload exactly as the document wrote it.
    pub(crate) original: Value,
    /// The payload restricted to what the document declares — what serializing back must produce.
    pub(crate) expected: Value,
    /// Whether the document's own example contradicts its own schema.
    ///
    /// 19 corpus documents carry such examples. A harness with no verdict for them reports 19
    /// failures that are findings about the vendor, not about progeny.
    pub(crate) vendor_defect: bool,
}

/// Why a position contributed no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Skipped {
    /// The position's type is arbitrary JSON, so a round trip through it asserts nothing.
    Opaque,
    /// The type is spelled at the use site rather than named, so a test cannot name it either.
    Unnamed,
    /// The type keeps members the schema does not declare, so "what a faithful round trip keeps"
    /// is not the pruned payload and this cannot say what it is.
    Captures,
}

/// Every example payload the API surface carries, with the skipped positions counted.
pub(crate) fn collect(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    contracts: &Contracts,
    model: &ApiModel,
) -> (Vec<Payload>, Vec<Skipped>) {
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    let collector = Collect {
        resolved,
        shapes,
        contracts,
    };

    for operation in model.operations() {
        let at = &operation.origin;
        // Every body that has a type, not only the JSON one. A multipart body's example is an
        // object describing its parts and deserializes into the same generated struct, so leaving
        // it out was coverage the gate could have had for nothing.
        if operation.body.as_ref().and_then(BodyContract::ty).is_some() {
            collector.position(
                &at.child("requestBody"),
                request_content(resolved, operation),
                &mut found,
                &mut skipped,
            );
        }
        let responses = at.child("responses");
        for (status, content) in response_content(resolved, operation) {
            collector.position(&responses.child(status), content, &mut found, &mut skipped);
        }
    }
    (found, skipped)
}

fn request_content<'a>(
    resolved: &'a ResolvedDocument,
    operation: &super::OperationContract,
) -> Vec<(&'a String, &'a MediaType)> {
    // Re-read from the document rather than from the contract: the contract holds the type, and
    // the examples live beside the schema that produced it.
    let Some(node) =
        document_operation(resolved, operation).and_then(|op| op.request_body.as_ref())
    else {
        return Vec::new();
    };
    let Some(body) = resolved.request_body(node) else {
        return Vec::new();
    };
    body.content.iter().flatten().collect()
}

fn response_content<'a>(
    resolved: &'a ResolvedDocument,
    operation: &super::OperationContract,
) -> Vec<(&'a str, Vec<(&'a String, &'a MediaType)>)> {
    let Some(responses) =
        document_operation(resolved, operation).and_then(|op| op.responses.as_ref())
    else {
        return Vec::new();
    };
    responses
        .statuses
        .iter()
        .filter_map(|(status, node)| {
            let response = resolved.response(node)?;
            Some((
                status.as_str(),
                response.content.iter().flatten().collect::<Vec<_>>(),
            ))
        })
        .collect()
}

/// The document node an operation contract came from.
///
/// The contract's origin is a JSON pointer of the form `/paths/<route>/<method>`, which is exactly
/// enough to find it again — and re-finding it is cheaper than carrying a borrow of the document
/// through the whole API model for the sake of one harness.
fn document_operation<'a>(
    resolved: &'a ResolvedDocument,
    operation: &super::OperationContract,
) -> Option<&'a crate::doc::Operation> {
    let (route, method) = match operation.origin.tokens() {
        [paths, route, method] if paths == "paths" => (route, method),
        _ => return None,
    };
    let item = resolved.document().paths.as_ref()?.items.get(route)?;
    let item = resolved.path_item(item)?;
    item.operations()
        .find_map(|(name, operation)| (name.slug() == method).then_some(operation))
}

struct Collect<'a> {
    resolved: &'a ResolvedDocument,
    shapes: &'a Shapes,
    contracts: &'a Contracts,
}

impl Collect<'_> {
    fn position(
        &self,
        at: &JsonPointer,
        content: Vec<(&String, &MediaType)>,
        found: &mut Vec<Payload>,
        skipped: &mut Vec<Skipped>,
    ) {
        for (media_type, entry) in content {
            let Some(id) = entry.schema else {
                continue;
            };
            let examples = examples_of(at, media_type, entry);
            if examples.is_empty() {
                continue;
            }
            let (type_name, shape) = match self.named(id) {
                Ok(pair) => pair,
                Err(reason) => {
                    skipped.extend(std::iter::repeat_n(reason, examples.len()));
                    continue;
                }
            };
            for (location, original) in examples {
                let Some(expected) = self.prune(&original, &shape) else {
                    skipped.push(Skipped::Captures);
                    continue;
                };
                found.push(Payload {
                    // Asked per example rather than read back out of the diagnostics, which
                    // aggregate and cap their locations.
                    vendor_defect: super::examples::contradiction(
                        self.resolved,
                        self.shapes,
                        id,
                        &original,
                    )
                    .is_some(),
                    location,
                    type_name: type_name.clone(),
                    original,
                    expected,
                });
            }
        }
    }

    /// The generated type a schema became, when it is one a test can name.
    fn named(&self, id: SchemaId) -> Result<(String, Shape), Skipped> {
        let key = crate::shape::key_of(self.resolved, id);
        let shape = self.shapes.get(&key).cloned().ok_or(Skipped::Unnamed)?;
        match self.contracts.type_of(&key) {
            Some(TypeRef::Named(index)) => {
                let contract = self.contracts.get(*index).ok_or(Skipped::Unnamed)?;
                Ok((contract.rust_name().as_str().to_owned(), shape))
            }
            Some(TypeRef::Value) | None => Err(Skipped::Opaque),
            Some(_) => Err(Skipped::Unnamed),
        }
    }

    /// The payload restricted to what the shape declares, or nothing when the type keeps more.
    ///
    /// One arm per shape, and deliberately not collapsed where two happen to agree: adding a shape
    /// should not compile until somebody has said what pruning a payload against it means, and a
    /// merged arm would quietly absorb the next one.
    fn prune(&self, value: &Value, shape: &Shape) -> Option<Value> {
        match shape {
            // Arbitrary JSON is carried whole, so nothing is pruned from it.
            Shape::Any => Some(value.clone()),
            Shape::Struct(structure) => {
                let members = value.as_object()?;
                if structure.extra != Extra::Denied && !matches!(structure.extra, Extra::Open) {
                    // A typed catch-all keeps undeclared members in a map, so the pruned payload is
                    // not what a round trip produces and this cannot say what is.
                    return None;
                }
                let mut out = Map::new();
                for field in &structure.fields {
                    let Some(present) = members.get(&field.wire) else {
                        continue;
                    };
                    // An optional field is an `Option` that is left out when it is `None`, so an
                    // explicit `null` comes back *absent*. That is the presence collapse, already
                    // reported as a `Degrade` against the position it costs — expecting the null
                    // back would make this gate red for a documented policy rather than for a
                    // defect, which is how a gate stops being read.
                    if !field.required && present.is_null() {
                        continue;
                    }
                    out.insert(field.wire.clone(), self.through(present, &field.shape)?);
                }
                Some(Value::Object(out))
            }
            Shape::Map { value: element } => {
                let members = value.as_object()?;
                let mut out = Map::new();
                for (key, member) in members {
                    let pruned = match element {
                        Some(element) => self.through(member, element)?,
                        None => member.clone(),
                    };
                    out.insert(key.clone(), pruned);
                }
                Some(Value::Object(out))
            }
            Shape::Array { item } => {
                let items = value.as_array()?;
                let mut out = Vec::with_capacity(items.len());
                for element in items {
                    out.push(match item {
                        Some(item) => self.through(element, item)?,
                        None => element.clone(),
                    });
                }
                Some(Value::Array(out))
            }
            Shape::FixedArray { item, .. } => {
                let items = value.as_array()?;
                let mut out = Vec::with_capacity(items.len());
                for element in items {
                    out.push(self.through(element, item)?);
                }
                Some(Value::Array(out))
            }
            Shape::Tuple { items, .. } => {
                let given = value.as_array()?;
                // A payload of a different length than the tuple is a vendor defect the
                // deserializer will report; there is nothing to prune it against.
                if given.len() != items.len() {
                    return Some(value.clone());
                }
                let mut out = Vec::with_capacity(given.len());
                for (element, item) in given.iter().zip(items) {
                    out.push(self.through(element, item)?);
                }
                Some(Value::Array(out))
            }
            Shape::Optional(inner) | Shape::Alias(inner) => {
                if value.is_null() {
                    return Some(Value::Null);
                }
                self.through(value, inner)
            }
            // Serde tries an untagged enum's variants in declaration order and takes the first
            // that deserializes, so the expectation is the payload pruned under that same first
            // branch. Keeping the payload whole instead would report every member the chosen
            // branch does not carry as a loss — which is a finding about the harness, not the code.
            Shape::Union(union) => {
                for variant in &union.variants {
                    let accepted = match &variant.shape {
                        ShapeRef::Key(key) => self.shapes.get(key).is_some_and(|shape| {
                            super::examples::accepts(self.resolved, self.shapes, value, shape)
                        }),
                        ShapeRef::Inline(shape) => {
                            super::examples::accepts(self.resolved, self.shapes, value, shape)
                        }
                    };
                    if accepted {
                        return self.through(value, &variant.shape);
                    }
                }
                // No branch accepts it. Deserializing will fail and say so, which is the finding —
                // and it is a finding about the document, which the example check has already made.
                Some(value.clone())
            }
            Shape::Null | Shape::Scalar(_) | Shape::Format(_) | Shape::StringEnum(_) => {
                Some(value.clone())
            }
        }
    }

    fn through(&self, value: &Value, reference: &ShapeRef) -> Option<Value> {
        match reference {
            ShapeRef::Key(key) => {
                let shape = self.shapes.get(key)?.clone();
                self.prune(value, &shape)
            }
            ShapeRef::Inline(shape) => self.prune(value, shape),
        }
    }
}

/// The examples one media type entry carries, with the pointer each was written at.
fn examples_of(at: &JsonPointer, media_type: &str, entry: &MediaType) -> Vec<(String, Value)> {
    let base = at.child("content").child(media_type);
    let mut out = Vec::new();
    if let Some(example) = &entry.example {
        out.push((base.child("example").to_string(), example.clone()));
    }
    let listed = base.child("examples");
    for (name, node) in entry.examples.iter().flatten() {
        if let MaybeRef::Item(example) = node
            && let Some(value) = &example.value
        {
            out.push((listed.child(name.clone()).to_string(), value.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::api::tests::model_of;
    use crate::config::Config;
    use crate::diag::Ctx;
    use crate::doc::parse as doc_parse;
    use crate::{contract, normalize, resolve, shape};

    /// Collect the payloads of a document the way the harness does.
    fn payloads_of(document: Value) -> Vec<super::Payload> {
        let config = Config::default();
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx).unwrap();
        let model = crate::api::build(&resolved, &shapes, &contracts, &config, &mut ctx).unwrap();
        super::collect(&resolved, &shapes, &contracts, &model).0
    }

    /// A document whose `200` response carries `schema` and `example`.
    fn responding(schema: Value, example: Value) -> Value {
        let mut media = serde_json::Map::new();
        media.insert("schema".to_owned(), schema);
        media.insert("example".to_owned(), example);
        let mut content = serde_json::Map::new();
        content.insert("application/json".to_owned(), Value::Object(media));
        let mut response = serde_json::Map::new();
        response.insert("description".to_owned(), json!("ok"));
        response.insert("content".to_owned(), Value::Object(content));
        let mut responses = serde_json::Map::new();
        responses.insert("200".to_owned(), Value::Object(response));
        let mut operation = serde_json::Map::new();
        operation.insert("operationId".to_owned(), json!("listPets"));
        operation.insert("responses".to_owned(), Value::Object(responses));
        let mut item = serde_json::Map::new();
        item.insert("get".to_owned(), Value::Object(operation));
        let mut paths = serde_json::Map::new();
        paths.insert("/pets".to_owned(), Value::Object(item));
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), Value::Object(paths));
        Value::Object(root)
    }

    #[test]
    fn an_example_is_paired_with_the_type_generated_for_its_position() {
        let found = payloads_of(responding(
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            json!({"name": "Rex"}),
        ));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].type_name, "ListPetsResponse200");
        assert_eq!(
            found[0].location,
            "/paths/~1pets/get/responses/200/content/application~1json/example"
        );
        assert_eq!(found[0].expected, json!({"name": "Rex"}));
    }

    #[test]
    fn a_member_the_schema_never_named_is_not_expected_back() {
        // The generated type ignores what it does not declare, so its absence afterwards is
        // correct. Expecting it back would fail every payload richer than its schema.
        let found = payloads_of(responding(
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            json!({"name": "Rex", "undeclared": 7}),
        ));
        assert_eq!(found[0].original, json!({"name": "Rex", "undeclared": 7}));
        assert_eq!(found[0].expected, json!({"name": "Rex"}));
    }

    #[test]
    fn an_explicit_null_in_an_optional_member_is_expected_to_come_back_absent() {
        // The presence collapse, stated as an expectation rather than discovered as a failure: an
        // optional member is an `Option` that is skipped when it is `None`.
        let found = payloads_of(responding(
            json!({"type": "object", "properties": {"name": {"type": ["string", "null"]}}}),
            json!({"name": null}),
        ));
        assert_eq!(found[0].expected, json!({}));

        // A *required* nullable member is always written, `null` included.
        let found = payloads_of(responding(
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {"name": {"type": ["string", "null"]}},
            }),
            json!({"name": null}),
        ));
        assert_eq!(found[0].expected, json!({"name": null}));
    }

    #[test]
    fn pruning_reaches_through_lists_and_named_references() {
        let found = payloads_of(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Page"},
                    "example": {"items": [{"name": "Rex", "undeclared": 1}], "extra": 2},
                }}}},
            }}},
            "components": {"schemas": {
                "Page": {"type": "object", "properties": {"items": {"type": "array", "items": {"$ref": "#/components/schemas/Pet"}}}},
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            }},
        }));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].type_name, "Page");
        assert_eq!(found[0].expected, json!({"items": [{"name": "Rex"}]}));
    }

    #[test]
    fn a_position_typed_as_arbitrary_json_contributes_nothing_to_check() {
        // `true` accepts everything, so it types as arbitrary JSON: a round trip through it is
        // `Value` in and `Value` out, which asserts nothing about the generated code.
        let document = responding(json!(true), json!({"anything": 1}));
        let config = Config::default();
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx).unwrap();
        let model = crate::api::build(&resolved, &shapes, &contracts, &config, &mut ctx).unwrap();
        let (found, skipped) = super::collect(&resolved, &shapes, &contracts, &model);
        assert!(found.is_empty());
        // Counted rather than dropped: a gate that omits silently reads as coverage it lacks.
        assert_eq!(skipped, [super::Skipped::Opaque]);
    }

    #[test]
    fn an_example_the_document_contradicts_carries_a_vendor_verdict() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {"name": {"type": "string"}},
        });
        let found = payloads_of(responding(schema.clone(), json!({"other": 1})));
        assert!(found[0].vendor_defect);
        // And the same schema with an example that agrees carries none, so the verdict is a
        // judgement about the example rather than a property of the position.
        let found = payloads_of(responding(schema, json!({"name": "Rex"})));
        assert!(!found[0].vendor_defect);

        // The verdict is asked per example, so it survives past the fifth: the class aggregates
        // and caps its related locations, and reading it back from there would be right about the
        // first few examples of a document and quietly wrong about the rest.
        let (_, diagnostics) = model_of(responding(
            json!({"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}),
            json!({"other": 1}),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::InvalidExample)
        );
    }
}
