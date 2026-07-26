//! Writing a schema back out as the JSON value it was read from.
//!
//! This is one half of the round-trip property that the whole model exists to satisfy, so it
//! mirrors [`super::parse`] member for member. Both halves are grouped the same way and in the
//! same order, which is the cheapest thing that makes a missing member visible in review; the
//! corpus round-trip is what makes it visible in CI.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    Discriminator, ExternalDocs, OneOrMany, Schema, SchemaId, SchemaObject, SchemaStore, TypeName,
    Xml,
};
use crate::value::Builder;

pub(crate) fn schema(store: &SchemaStore, id: SchemaId) -> Value {
    match store.get(id) {
        Schema::Bool(flag) => Value::Bool(*flag),
        Schema::Object(object) => object_to_value(store, object),
    }
}

pub(crate) fn schema_map(store: &SchemaStore, entries: &BTreeMap<String, SchemaId>) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(name, id)| (name.clone(), schema(store, *id)))
            .collect(),
    )
}

fn schema_array(store: &SchemaStore, ids: &[SchemaId]) -> Value {
    Value::Array(ids.iter().map(|id| schema(store, *id)).collect())
}

fn object_to_value(store: &SchemaStore, object: &SchemaObject) -> Value {
    let mut out = Builder::new();
    core(&mut out, store, object);
    applicators(&mut out, store, object);
    validation(&mut out, object);
    annotation(&mut out, store, object);
    openapi(&mut out, object);
    out.extend(&object.unknown);
    out.finish()
}

fn core(out: &mut Builder, store: &SchemaStore, object: &SchemaObject) {
    out.set("$schema", object.schema_dialect.clone());
    out.set("$id", object.id.clone());
    out.set("$anchor", object.anchor.clone());
    out.set("$dynamicAnchor", object.dynamic_anchor.clone());
    out.set("$ref", object.reference.clone());
    out.set("$dynamicRef", object.dynamic_reference.clone());
    out.set("$comment", object.comment.clone());
    out.set_with("$defs", object.defs.as_ref(), |defs| {
        schema_map(store, defs)
    });
    out.set_with("definitions", object.definitions.as_ref(), |defs| {
        schema_map(store, defs)
    });
}

fn applicators(out: &mut Builder, store: &SchemaStore, object: &SchemaObject) {
    out.set_with("allOf", object.all_of.as_deref(), |ids| {
        schema_array(store, ids)
    });
    out.set_with("anyOf", object.any_of.as_deref(), |ids| {
        schema_array(store, ids)
    });
    out.set_with("oneOf", object.one_of.as_deref(), |ids| {
        schema_array(store, ids)
    });
    out.set_with("not", object.not, |id| schema(store, id));
    out.set_with("if", object.if_schema, |id| schema(store, id));
    out.set_with("then", object.then_schema, |id| schema(store, id));
    out.set_with("else", object.else_schema, |id| schema(store, id));
    out.set_with("dependentSchemas", object.dependent_schemas.as_ref(), |m| {
        schema_map(store, m)
    });
    out.set_with("properties", object.properties.as_ref(), |m| {
        schema_map(store, m)
    });
    out.set_with(
        "patternProperties",
        object.pattern_properties.as_ref(),
        |m| schema_map(store, m),
    );
    out.set_with("additionalProperties", object.additional_properties, |id| {
        schema(store, id)
    });
    out.set_with("propertyNames", object.property_names, |id| {
        schema(store, id)
    });
    out.set_with("items", object.items, |id| schema(store, id));
    out.set_with("prefixItems", object.prefix_items.as_deref(), |ids| {
        schema_array(store, ids)
    });
    out.set_with("contains", object.contains, |id| schema(store, id));
    out.set_with("unevaluatedItems", object.unevaluated_items, |id| {
        schema(store, id)
    });
    out.set_with(
        "unevaluatedProperties",
        object.unevaluated_properties,
        |id| schema(store, id),
    );
}

fn validation(out: &mut Builder, object: &SchemaObject) {
    out.set_with("type", object.types.as_ref(), type_names_to_value);
    out.set_with("enum", object.enumeration.clone(), Value::Array);
    out.set("const", object.constant.clone());
    out.set_with("multipleOf", object.multiple_of.clone(), Value::Number);
    out.set_with("maximum", object.maximum.clone(), Value::Number);
    out.set_with(
        "exclusiveMaximum",
        object.exclusive_maximum.clone(),
        Value::Number,
    );
    out.set_with("minimum", object.minimum.clone(), Value::Number);
    out.set_with(
        "exclusiveMinimum",
        object.exclusive_minimum.clone(),
        Value::Number,
    );
    out.set_with("maxLength", object.max_length.clone(), Value::Number);
    out.set_with("minLength", object.min_length.clone(), Value::Number);
    out.set("pattern", object.pattern.clone());
    out.set_with("maxItems", object.max_items.clone(), Value::Number);
    out.set_with("minItems", object.min_items.clone(), Value::Number);
    out.set("uniqueItems", object.unique_items);
    out.set_with("maxContains", object.max_contains.clone(), Value::Number);
    out.set_with("minContains", object.min_contains.clone(), Value::Number);
    out.set_with(
        "maxProperties",
        object.max_properties.clone(),
        Value::Number,
    );
    out.set_with(
        "minProperties",
        object.min_properties.clone(),
        Value::Number,
    );
    out.set_array("required", object.required.as_deref(), |name| {
        Value::String(name.clone())
    });
    out.set_map(
        "dependentRequired",
        object.dependent_required.as_ref(),
        |names| {
            Value::Array(
                names
                    .iter()
                    .map(|name| Value::String(name.clone()))
                    .collect(),
            )
        },
    );
}

fn annotation(out: &mut Builder, store: &SchemaStore, object: &SchemaObject) {
    out.set("title", object.title.clone());
    out.set("description", object.description.clone());
    out.set("default", object.default.clone());
    out.set("deprecated", object.deprecated);
    out.set("readOnly", object.read_only);
    out.set("writeOnly", object.write_only);
    out.set_with("examples", object.examples.clone(), Value::Array);
    out.set("format", object.format.clone());
    out.set("contentEncoding", object.content_encoding.clone());
    out.set("contentMediaType", object.content_media_type.clone());
    out.set_with("contentSchema", object.content_schema, |id| {
        schema(store, id)
    });
}

fn openapi(out: &mut Builder, object: &SchemaObject) {
    out.set_with(
        "discriminator",
        object.discriminator.as_ref(),
        discriminator_to_value,
    );
    out.set_with("xml", object.xml.as_ref(), xml_to_value);
    out.set_with(
        "externalDocs",
        object.external_docs.as_ref(),
        external_docs_to_value,
    );
}

fn type_names_to_value(types: &OneOrMany<TypeName>) -> Value {
    match types {
        OneOrMany::One(name) => Value::String(name.as_str().to_owned()),
        OneOrMany::Many(names) => Value::Array(
            names
                .iter()
                .map(|name| Value::String(name.as_str().to_owned()))
                .collect(),
        ),
    }
}

fn discriminator_to_value(discriminator: &Discriminator) -> Value {
    let mut out = Builder::new();
    out.set("propertyName", discriminator.property_name.clone());
    out.set_map("mapping", discriminator.mapping.as_ref(), |target| {
        Value::String(target.clone())
    });
    out.extend(&discriminator.extensions);
    out.finish()
}

fn xml_to_value(xml: &Xml) -> Value {
    let mut out = Builder::new();
    out.set("name", xml.name.clone());
    out.set("namespace", xml.namespace.clone());
    out.set("prefix", xml.prefix.clone());
    out.set("attribute", xml.attribute);
    out.set("wrapped", xml.wrapped);
    out.extend(&xml.extensions);
    out.finish()
}

/// Shared with the document model, which has four `externalDocs` positions of its own.
pub(crate) fn external_docs_to_value(docs: &ExternalDocs) -> Value {
    let mut out = Builder::new();
    out.set("description", docs.description.clone());
    out.set("url", docs.url.clone());
    out.extend(&docs.extensions);
    out.finish()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::{SchemaStore, parse};
    use super::schema;
    use crate::diag::Ctx;

    fn round_trip(value: Value) -> Value {
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        let id = parse::schema(value, &mut store, &mut ctx).unwrap();
        schema(&store, id)
    }

    /// Every keyword the model claims to hold, in one schema.
    ///
    /// Written as text rather than through `json!` so that the number literals in it are the
    /// literals under test, and so that adding a keyword to the model without adding it here is
    /// visible as an unmentioned keyword rather than hidden in a macro.
    const EVERY_KEYWORD: &str = r##"{
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$id": "https://example.test/a",
      "$anchor": "a",
      "$dynamicAnchor": "meta",
      "$ref": "#/$defs/Inner",
      "$dynamicRef": "#meta",
      "$comment": "c",
      "$defs": {"Inner": {"type": "string"}},
      "definitions": {"Legacy": {"type": "string"}},
      "allOf": [{"type": "object"}],
      "anyOf": [true, false],
      "oneOf": [{"const": 1}],
      "not": {"type": "null"},
      "if": {"required": ["a"]},
      "then": {"required": ["b"]},
      "else": true,
      "dependentSchemas": {"a": {"required": ["b"]}},
      "properties": {"a": {"type": "string"}, "b": false},
      "patternProperties": {"^x-": {"type": "string"}},
      "additionalProperties": false,
      "propertyNames": {"pattern": "^[a-z]+$"},
      "items": {"type": "integer"},
      "prefixItems": [{"type": "string"}, {"type": "number"}],
      "contains": {"type": "string"},
      "unevaluatedItems": false,
      "unevaluatedProperties": true,
      "type": ["object", "null"],
      "enum": [1, "a", null],
      "const": 3,
      "multipleOf": 2,
      "maximum": 10,
      "exclusiveMaximum": 11,
      "minimum": 0,
      "exclusiveMinimum": -1,
      "maxLength": 5,
      "minLength": 1,
      "pattern": "^a$",
      "maxItems": 3,
      "minItems": 1,
      "uniqueItems": true,
      "maxContains": 2,
      "minContains": 1,
      "maxProperties": 4,
      "minProperties": 1,
      "required": ["a"],
      "dependentRequired": {"a": ["b", "c"]},
      "title": "T",
      "description": "D",
      "default": {"a": null},
      "deprecated": false,
      "readOnly": true,
      "writeOnly": false,
      "examples": [1, 2],
      "format": "uuid",
      "contentEncoding": "base64",
      "contentMediaType": "image/png",
      "contentSchema": {"type": "string"},
      "discriminator": {"propertyName": "kind", "mapping": {"a": "#/x"}, "x-note": 1},
      "xml": {"name": "a", "namespace": "urn:x", "prefix": "p", "attribute": true, "wrapped": false},
      "externalDocs": {"description": "docs", "url": "https://example.test", "x-e": true},
      "x-vendor": {"anything": [1, {"deep": true}]},
      "unmodelledKeyword": "kept"
    }"##;

    #[test]
    fn every_keyword_survives_the_round_trip() {
        let original: Value = serde_json::from_str(EVERY_KEYWORD).unwrap();
        assert_eq!(round_trip(original.clone()), original);
    }

    #[test]
    fn every_keyword_the_model_holds_is_covered_by_that_fixture() {
        // A keyword read into a typed field but never written back would round-trip only by
        // accident; a keyword neither read nor written would sit in `unknown` and round-trip
        // silently. The fixture is the guard against both, so it has to actually reach the
        // typed fields.
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        let id = parse::schema(
            serde_json::from_str(EVERY_KEYWORD).unwrap(),
            &mut store,
            &mut ctx,
        )
        .unwrap();
        let object = match store.get(id) {
            crate::schema::Schema::Object(object) => object,
            crate::schema::Schema::Bool(_) => panic!("expected an object schema"),
        };
        // Only the two deliberately unmodelled members.
        assert_eq!(
            object.unknown.keys().collect::<Vec<_>>(),
            ["unmodelledKeyword", "x-vendor"]
        );
    }

    #[test]
    fn walking_children_reaches_every_schema_in_the_store() {
        // `SchemaObject::children` is the only description of the schema graph's shape, and
        // resolution, cycle detection and classification all read the graph through it. A
        // subschema field it forgot would simply be invisible to all three, with no other
        // symptom — so the fixture that holds every keyword is asserted to be fully reachable.
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        let root = parse::schema(
            serde_json::from_str(EVERY_KEYWORD).unwrap(),
            &mut store,
            &mut ctx,
        )
        .unwrap();

        let mut seen = std::collections::BTreeSet::from([root]);
        let mut queue = vec![root];
        while let Some(id) = queue.pop() {
            if let crate::schema::Schema::Object(object) = store.get(id).clone() {
                object.children(|child| {
                    if seen.insert(child) {
                        queue.push(child);
                    }
                });
            }
        }
        assert_eq!(seen.len(), store.len());
    }

    #[test]
    fn the_written_form_of_type_is_preserved() {
        assert_eq!(
            round_trip(json!({"type": "string"})),
            json!({"type": "string"})
        );
        assert_eq!(
            round_trip(json!({"type": ["string"]})),
            json!({"type": ["string"]})
        );
    }

    #[test]
    fn number_literals_are_preserved() {
        let original: Value =
            serde_json::from_str(r#"{"maximum": 1.0, "minimum": 1, "multipleOf": 1e2}"#).unwrap();
        let text = serde_json::to_string(&round_trip(original)).unwrap();
        assert_eq!(text, r#"{"maximum":1.0,"minimum":1,"multipleOf":1e+2}"#);
    }

    #[test]
    fn empty_collections_are_not_the_same_as_absent_ones() {
        assert_eq!(round_trip(json!({"required": []})), json!({"required": []}));
        assert_eq!(
            round_trip(json!({"properties": {}})),
            json!({"properties": {}})
        );
        assert_eq!(round_trip(json!({})), json!({}));
    }

    #[test]
    fn a_malformed_keyword_still_round_trips() {
        assert_eq!(
            round_trip(json!({"required": "a", "maximum": "big"})),
            json!({"required": "a", "maximum": "big"})
        );
    }

    #[test]
    fn an_explicit_null_default_is_kept() {
        assert_eq!(
            round_trip(json!({"default": null})),
            json!({"default": null})
        );
    }
}
