//! The breakage catalogue, kept honest by running it.
//!
//! [`crate::BreakageClass`] is closed and has no catch-all variant, so a newly observed way for a
//! description to be broken has to become a variant — which forces a decision about which action it
//! gets and which mechanism implements it. That much the compiler holds. What it does not hold is
//! the *third* decision the catalogue demands: which fixture pins the class, so that the mechanism
//! is known to work rather than merely known to have been written.
//!
//! This module is that gate. Every class is either produced by a document here, or explicitly
//! recorded as arriving with a later stage's surface — a class about routes cannot be exercised
//! before there is a router. The list of classes is read out of the enum itself rather than kept by
//! hand, so a variant added without a fixture fails this test instead of quietly passing it.

#![cfg(test)]

use serde_json::{Value, json};

use crate::{BreakageClass, Config, Diagnostic, generate};

/// What proves a catalogue row does what it says.
enum Wiring {
    /// A document that produces this class today.
    Fixture(Value),
    /// Nothing can produce it yet, and this says what has to exist first.
    Arrives(&'static str),
    /// The architecture dissolves the class rather than handling it, so the fixture asserts its
    /// *absence*: the document that would have triggered it in the predecessor goes through clean.
    Dissolved(Value),
}

/// The one decision per class this module exists to force.
///
/// Exhaustive, with no wildcard arm, for the same reason the eligibility function is: adding a
/// breakage class should not compile until someone has said how it is proven.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per breakage class: the table is the deliverable, and lifting the fixtures \
              out to shorten it would put the documents somewhere the exhaustive match no longer \
              reaches"
)]
fn wiring(class: BreakageClass) -> Wiring {
    match class {
        BreakageClass::MalformedMember => Wiring::Fixture(document(json!({
            "Pet": {"type": "object", "description": {"not": "a string"}},
        }))),
        BreakageClass::UnknownSchemaType => Wiring::Fixture(document(json!({
            "Odd": {"type": "sasquatch"},
        }))),
        BreakageClass::UnsupportedConstruct => Wiring::Fixture(document(json!({
            "Odd": {"type": "object", "properties": {"a": {"type": "string"}}, "not": {"required": ["b"]}},
        }))),
        BreakageClass::IrreconcilableAllOf => Wiring::Fixture(document(json!({
            "Impossible": {"allOf": [{"type": "string"}, {"type": "integer"}]},
        }))),
        BreakageClass::PresenceCollapse => Wiring::Fixture(document(json!({
            "Thing": {"type": "object", "properties": {"both": {"type": ["string", "null"]}}},
        }))),
        BreakageClass::InvalidDefault => Wiring::Fixture(document(json!({
            "Thing": {"type": "object", "properties": {"count": {"type": "integer", "default": "seven"}}},
        }))),
        BreakageClass::CollidingTypeName => Wiring::Fixture(document(json!({
            "Thing": {
                "type": "object",
                "properties": {
                    "a": {"type": "object", "properties": {"inner": {"type": "object", "properties": {"x": {"type": "string"}}}}},
                    "a_inner": {"type": "object", "properties": {"y": {"type": "string"}}},
                },
            },
        }))),
        BreakageClass::DanglingRef => Wiring::Fixture(document(json!({
            "Thing": {
                "type": "object",
                "properties": {"gone": {"$ref": "#/components/schemas/NotThere"}},
            },
        }))),
        BreakageClass::WildUnion => Wiring::Fixture(document(json!({
            "A": {"type": "object", "properties": {"shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"shared": {"type": "string"}, "extra": {"type": "string"}}},
            "Either": {"oneOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/B"},
            ]},
        }))),
        BreakageClass::DiscriminatorEdgeCase => Wiring::Fixture(document(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            // The use that makes taking `kind` off `A` a loss, and the union unrepresentable.
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        }))),
        BreakageClass::MultiParentDiscriminator => Wiring::Fixture(document(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "First": {
                "oneOf": [{"$ref": "#/components/schemas/A"}, {"$ref": "#/components/schemas/B"}],
                "discriminator": {"propertyName": "kind"},
            },
            "Second": {
                "oneOf": [{"$ref": "#/components/schemas/B"}, {"$ref": "#/components/schemas/A"}],
                "discriminator": {"propertyName": "kind"},
            },
        }))),
        BreakageClass::LegacyTupleItems => Wiring::Fixture(document(json!({
            "Pair": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}]},
        }))),
        BreakageClass::LegacyExclusiveBound => Wiring::Fixture(document(json!({
            "Bounded": {"type": "integer", "minimum": 1, "exclusiveMinimum": true},
        }))),
        // A 3.1 document writing 3.0's `format: binary`, which is how a multipart body says which
        // property is a file. 110 occurrences across 15 documents that declare 3.1.
        BreakageClass::LegacyStringFormat => Wiring::Fixture(document(json!({
            "Upload": {
                "type": "object",
                "properties": {"file": {"type": "string", "format": "binary"}},
            },
        }))),
        BreakageClass::NullableUnionBranch => Wiring::Fixture(dialect_30(json!({
            "Usage": {"type": "object", "properties": {"total": {"type": "integer"}}},
            "MaybeUsage": {"anyOf": [{"$ref": "#/components/schemas/Usage"}, {"nullable": true}]},
        }))),
        BreakageClass::UnsatisfiableDerive => Wiring::Fixture(document(json!({
            "Fuzzy": {"type": "object", "properties": {"ratio": {"type": "number"}}},
        }))),
        BreakageClass::UnsupportedDialect => Wiring::Fixture(document_with(
            json!({"Thing": {"type": "string", "$schema": "http://json-schema.org/draft-07/schema#"}}),
            "jsonSchemaDialect",
            json!("https://example.invalid/dialect"),
        )),
        BreakageClass::DynamicScoping => Wiring::Fixture(document(json!({
            "Thing": {"type": "object", "properties": {"self": {"$dynamicRef": "#node"}}},
        }))),
        BreakageClass::NonFiniteNumber => Wiring::Arrives(
            "a YAML `.inf` / `.nan` scalar, which the loader produces; the corpus holds none, so \
             the fixture lives with the loader's own tests rather than being a document here",
        ),
        BreakageClass::MissingFinalLineBreak => Wiring::Arrives(
            "a YAML document whose bytes end inside a block scalar, which is a property of the \
             bytes rather than of the model; the loader's own tests hold it",
        ),
        BreakageClass::InvalidExample => Wiring::Fixture(paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": {"200": {
                        "description": "ok",
                        "content": {"application/json": {
                            "schema": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}},
                            "example": {"nickname": "Rex"},
                        }},
                    }},
                },
            },
        }))),
        BreakageClass::MultiMediaType => Wiring::Fixture(paths(json!({
            "/pets": {
                "post": {
                    "operationId": "createPet",
                    "requestBody": {"content": {
                        "application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}},
                        "application/xml": {"schema": {"type": "string"}},
                    }},
                    "responses": {"201": {"description": "made"}},
                },
            },
        }))),
        BreakageClass::CollidingOperationId => Wiring::Fixture(paths(json!({
            "/a": {"get": {"operationId": "list-pets", "responses": {"200": {"description": "ok"}}}},
            "/b": {"get": {"operationId": "list_pets", "responses": {"200": {"description": "ok"}}}},
        }))),
        BreakageClass::QuerySerializationStyle => Wiring::Fixture(paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    // deepObject over an array: the one combination the specification names and
                    // explicitly declines to define.
                    "parameters": [{
                        "name": "filter", "in": "query", "style": "deepObject",
                        "schema": {"type": "array", "items": {"type": "string"}},
                    }],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        }))),
        // Half of this class: a template no parameter can fill, which is a property of one
        // operation. The other half — two routes colliding under a router's matching rules — is a
        // property of a router, and there is none before stage 7.
        // Both halves, in one fixture. The first path names a variable no parameter declares, so
        // the *client* cannot build its URL and the operation is skipped; the second and third are
        // one route under two names, which only a *router* can object to, so they keep their client
        // methods and the second loses its server handler. The two halves landed a stage apart and
        // are two different sentences about two different things.
        BreakageClass::UnregistrableRoute => Wiring::Fixture(paths(json!({
            "/pets/{petId}": {
                "get": {"operationId": "getPet", "responses": {"200": {"description": "ok"}}},
            },
            "/toys/{toyId}": {
                "get": {
                    "operationId": "getToy",
                    "parameters": [{"name": "toyId", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
            "/toys/{toyName}": {
                "get": {
                    "operationId": "getToyByName",
                    "parameters": [{"name": "toyName", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        }))),
    }
}

/// The classes this architecture dissolves rather than handles, and the documents that prove it.
///
/// These are not in [`wiring`] because they have no variant to be: the catalogue in `05` keeps
/// them as rows marked "—" precisely so the dissolution is verified rather than asserted, and a
/// verification of an absence needs a document, not a class.
fn dissolved() -> Vec<(&'static str, Wiring)> {
    vec![(
        // The predecessor expanded a referenced enum inline, so a schema reaching itself through
        // one recursed forever. An arena with node identity has nothing to expand.
        "recursive internal enum expansion",
        Wiring::Dissolved(document(json!({
            "Node": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["leaf", "branch"]},
                    "children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}},
                },
            },
        }))),
    )]
}

#[test]
fn every_breakage_class_is_pinned_by_a_fixture_or_dated() {
    let mut undated = Vec::new();
    for class in all_classes() {
        match wiring(class) {
            Wiring::Fixture(document) => {
                let produced = diagnostics_of(&document);
                assert!(
                    produced.iter().any(|found| found.class() == class),
                    "the fixture for `{class}` produced {:?} instead",
                    produced
                        .iter()
                        .map(|found| found.class().slug())
                        .collect::<Vec<_>>()
                );
            }
            Wiring::Arrives(reason) => undated.push(format!("{class}: waiting on {reason}")),
            Wiring::Dissolved(document) => {
                assert!(
                    !diagnostics_of(&document)
                        .iter()
                        .any(|found| found.class() == class),
                    "`{class}` is supposed to be dissolved, and something reported it"
                );
            }
        }
    }
    // Printed rather than asserted away: the point is that the list shrinks as stages land, and a
    // reader of the test output can see exactly what is still owed.
    for line in &undated {
        println!("not yet exercisable — {line}");
    }
}

#[test]
fn the_classes_this_architecture_dissolves_stay_dissolved() {
    for (name, wiring) in dissolved() {
        let Wiring::Dissolved(document) = wiring else {
            panic!("{name} should be recorded as dissolved");
        };
        let output = generate(&serde_json::to_vec(&document).unwrap(), &Config::default())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        // The dissolution's whole claim: the construct generates, rather than being handled.
        assert!(
            output
                .files
                .values()
                .any(|text| text.contains("pub struct")),
            "{name} produced no types at all"
        );
    }
}

/// Every variant of the closed enum, read out of the enum rather than listed by hand.
///
/// The derived `Deserialize` names all of them when it rejects one, which makes the list a
/// property of the type instead of a second copy that can fall behind it.
fn all_classes() -> Vec<BreakageClass> {
    let error = serde_json::from_value::<BreakageClass>(json!("no-such-class"))
        .expect_err("a class that does not exist should not deserialize");
    let message = error.to_string();
    let listed = message
        .split_once("expected one of ")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("the variant list moved: {message}"));
    let classes: Vec<BreakageClass> = listed
        .split(", ")
        .filter_map(|name| name.trim().split('`').nth(1))
        .filter_map(|slug| serde_json::from_value(json!(slug)).ok())
        .collect();
    assert!(
        classes.len() > 20,
        "only found {} classes in {message}",
        classes.len()
    );
    classes
}

fn diagnostics_of(document: &Value) -> Vec<Diagnostic> {
    let bytes = serde_json::to_vec(document).expect("a fixture should serialize");
    // Every optional derive requested crate-wide, so the one class that is about the caller's
    // configuration rather than the document has something to be unsatisfiable about.
    let config = Config {
        derives: [crate::Derive::Eq].into_iter().collect(),
        ..Config::default()
    };
    match generate(&bytes, &config) {
        Ok(output) => output.diagnostics,
        Err(error) => panic!("a fixture should generate: {error}"),
    }
}

fn document(schemas: Value) -> Value {
    declaring("3.1.0", schemas)
}

/// The same, declaring 3.0, for the classes that are about reading the older dialect.
fn dialect_30(schemas: Value) -> Value {
    declaring("3.0.3", schemas)
}

/// Built member by member rather than with `json!`, which would only borrow `schemas`.
fn declaring(version: &str, schemas: Value) -> Value {
    let mut components = serde_json::Map::new();
    components.insert("schemas".to_owned(), schemas);
    let mut root = serde_json::Map::new();
    root.insert("openapi".to_owned(), Value::String(version.to_owned()));
    root.insert("paths".to_owned(), Value::Object(serde_json::Map::new()));
    root.insert("components".to_owned(), Value::Object(components));
    Value::Object(root)
}

/// A document whose findings are about its operations rather than its schemas.
fn paths(paths: Value) -> Value {
    let mut root = serde_json::Map::new();
    root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
    root.insert("paths".to_owned(), paths);
    Value::Object(root)
}

fn document_with(schemas: Value, key: &str, value: Value) -> Value {
    let mut root = document(schemas);
    if let Some(map) = root.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
    root
}
