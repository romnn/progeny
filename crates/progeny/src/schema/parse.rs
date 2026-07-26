//! Reading a schema out of a JSON value and into the store.
//!
//! Every function here is total for the values its caller admits: a schema object with a
//! nonsense member produces a schema plus a diagnostic, never a failure. The keyword groups
//! are read by separate functions purely so each stays small enough to read.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    Discriminator, ExternalDocs, OneOrMany, Schema, SchemaId, SchemaObject, SchemaStore, TypeName,
    Xml,
};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};
use crate::value::Members;

/// Read one schema, which is either a boolean or an object.
///
/// Anything else has no schema meaning; the caller decides what to do with it, because only the
/// caller knows which member it came from and can preserve it there.
pub(crate) fn schema(
    value: Value,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) -> Result<SchemaId, Value> {
    match value {
        Value::Bool(flag) => Ok(store.insert(Schema::Bool(flag), ctx.here().clone())),
        Value::Object(map) => {
            let object = object(Members::new(map), store, ctx);
            // After reading the children, so `ctx` is back at this schema's own position. The
            // children hold lower ids than their parent, which nothing depends on.
            Ok(store.insert(Schema::Object(Box::new(object)), ctx.here().clone()))
        }
        other => Err(other),
    }
}

/// Read a member whose value is a schema.
pub(crate) fn member(
    node: &mut Members,
    key: &str,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) -> Option<SchemaId> {
    node.typed(key, ctx, "a schema", |value, ctx| schema(value, store, ctx))
}

/// Read a member whose value is an array of schemas.
pub(crate) fn member_array(
    node: &mut Members,
    key: &str,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) -> Option<Vec<SchemaId>> {
    node.typed(key, ctx, "an array of schemas", |value, ctx| match value {
        Value::Array(items) if items.iter().all(is_schema) => Ok(items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                ctx.scoped(&index.to_string(), |ctx| schema(item, store, ctx).ok())
            })
            .collect()),
        other => Err(other),
    })
}

/// Read a member whose value is an object mapping names to schemas.
pub(crate) fn member_map(
    node: &mut Members,
    key: &str,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) -> Option<BTreeMap<String, SchemaId>> {
    node.typed(
        key,
        ctx,
        "an object whose members are schemas",
        |value, ctx| match value {
            Value::Object(map) if map.values().all(is_schema) => Ok(map
                .into_iter()
                .filter_map(|(name, item)| {
                    let id = ctx.scoped(&name, |ctx| schema(item, store, ctx).ok())?;
                    Some((name, id))
                })
                .collect()),
            other => Err(other),
        },
    )
}

fn is_schema(value: &Value) -> bool {
    value.is_object() || value.is_boolean()
}

fn object(mut node: Members, store: &mut SchemaStore, ctx: &mut Ctx) -> SchemaObject {
    let mut schema = SchemaObject::default();
    core(&mut node, &mut schema, store, ctx);
    applicators(&mut node, &mut schema, store, ctx);
    validation(&mut node, &mut schema, ctx);
    annotation(&mut node, &mut schema, store, ctx);
    openapi(&mut node, &mut schema, ctx);
    schema.unknown = node.rest();
    schema
}

/// The dialects progeny reads as 2020-12 without comment.
///
/// Real documents that set `$schema` at all set it to one of these; anything else is a claim
/// progeny cannot honour and therefore has to surface.
const KNOWN_DIALECTS: [&str; 2] = [
    "https://json-schema.org/draft/2020-12/schema",
    "https://spec.openapis.org/oas/3.1/dialect/base",
];

fn core(node: &mut Members, schema: &mut SchemaObject, store: &mut SchemaStore, ctx: &mut Ctx) {
    schema.schema_dialect = node.string("$schema", ctx);
    if let Some(dialect) = &schema.schema_dialect {
        let bare = dialect.trim_end_matches('#');
        if !KNOWN_DIALECTS.contains(&bare) {
            let location = ctx.child("$schema");
            ctx.report(Diagnostic::new(
                BreakageClass::UnsupportedDialect,
                Action::Warn,
                location,
                format!("`$schema: {dialect}` names a dialect progeny does not implement; read the schema as 2020-12"),
            ));
        }
    }

    schema.id = node.string("$id", ctx);
    schema.anchor = node.string("$anchor", ctx);
    schema.dynamic_anchor = node.string("$dynamicAnchor", ctx);
    schema.reference = node.string("$ref", ctx);
    schema.dynamic_reference = node.string("$dynamicRef", ctx);
    schema.comment = node.string("$comment", ctx);
    schema.defs = member_map(node, "$defs", store, ctx);
    schema.definitions = member_map(node, "definitions", store, ctx);

    // Dynamic scoping is held exactly as written and then resolved as though it were the plain
    // form. Building 2020-12's dynamic-scope machinery for a construct no published OpenAPI
    // document uses would be speculative generality; saying so out loud is the honest
    // alternative to either silently misresolving it or refusing the document.
    for (keyword, plain, present) in [
        ("$dynamicRef", "$ref", schema.dynamic_reference.is_some()),
        ("$dynamicAnchor", "$anchor", schema.dynamic_anchor.is_some()),
    ] {
        if present {
            let location = ctx.child(keyword);
            ctx.report(Diagnostic::new(
                BreakageClass::DynamicScoping,
                Action::Degrade,
                location,
                format!(
                    "`{keyword}` is resolved as though it were `{plain}`, so a schema relying on \
                     dynamic scoping resolves to the lexically nearest definition instead"
                ),
            ));
        }
    }
}

fn applicators(
    node: &mut Members,
    schema: &mut SchemaObject,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) {
    schema.all_of = member_array(node, "allOf", store, ctx);
    schema.any_of = member_array(node, "anyOf", store, ctx);
    schema.one_of = member_array(node, "oneOf", store, ctx);
    schema.not = member(node, "not", store, ctx);
    schema.if_schema = member(node, "if", store, ctx);
    schema.then_schema = member(node, "then", store, ctx);
    schema.else_schema = member(node, "else", store, ctx);
    schema.dependent_schemas = member_map(node, "dependentSchemas", store, ctx);
    schema.properties = member_map(node, "properties", store, ctx);
    schema.pattern_properties = member_map(node, "patternProperties", store, ctx);
    schema.additional_properties = member(node, "additionalProperties", store, ctx);
    schema.property_names = member(node, "propertyNames", store, ctx);
    schema.items = member(node, "items", store, ctx);
    schema.prefix_items = member_array(node, "prefixItems", store, ctx);
    schema.contains = member(node, "contains", store, ctx);
    schema.unevaluated_items = member(node, "unevaluatedItems", store, ctx);
    schema.unevaluated_properties = member(node, "unevaluatedProperties", store, ctx);
}

fn validation(node: &mut Members, schema: &mut SchemaObject, ctx: &mut Ctx) {
    schema.types = types(node, ctx);
    schema.enumeration = node.value_array("enum", ctx);
    schema.constant = node.value("const");
    schema.multiple_of = node.number("multipleOf", ctx);
    schema.maximum = node.number("maximum", ctx);
    schema.exclusive_maximum = node.number("exclusiveMaximum", ctx);
    schema.minimum = node.number("minimum", ctx);
    schema.exclusive_minimum = node.number("exclusiveMinimum", ctx);
    schema.max_length = node.number("maxLength", ctx);
    schema.min_length = node.number("minLength", ctx);
    schema.pattern = node.string("pattern", ctx);
    schema.max_items = node.number("maxItems", ctx);
    schema.min_items = node.number("minItems", ctx);
    schema.unique_items = node.bool("uniqueItems", ctx);
    schema.max_contains = node.number("maxContains", ctx);
    schema.min_contains = node.number("minContains", ctx);
    schema.max_properties = node.number("maxProperties", ctx);
    schema.min_properties = node.number("minProperties", ctx);
    schema.required = node.string_array("required", ctx);
    schema.dependent_required =
        node.string_array_map("dependentRequired", ctx, "an object of arrays of strings");
}

fn annotation(
    node: &mut Members,
    schema: &mut SchemaObject,
    store: &mut SchemaStore,
    ctx: &mut Ctx,
) {
    schema.title = node.string("title", ctx);
    schema.description = node.string("description", ctx);
    schema.default = node.value("default");
    schema.deprecated = node.bool("deprecated", ctx);
    schema.read_only = node.bool("readOnly", ctx);
    schema.write_only = node.bool("writeOnly", ctx);
    schema.examples = node.value_array("examples", ctx);
    schema.format = node.string("format", ctx);
    schema.content_encoding = node.string("contentEncoding", ctx);
    schema.content_media_type = node.string("contentMediaType", ctx);
    schema.content_schema = member(node, "contentSchema", store, ctx);
}

fn openapi(node: &mut Members, schema: &mut SchemaObject, ctx: &mut Ctx) {
    schema.discriminator = node.object("discriminator", ctx, |mut inner, ctx| {
        let mut discriminator = Discriminator {
            property_name: inner.string("propertyName", ctx),
            mapping: inner.string_map("mapping", ctx, "an object of strings"),
            extensions: BTreeMap::new(),
        };
        discriminator.extensions = inner.rest();
        discriminator
    });
    schema.xml = node.object("xml", ctx, |mut inner, ctx| {
        let mut xml = Xml {
            name: inner.string("name", ctx),
            namespace: inner.string("namespace", ctx),
            prefix: inner.string("prefix", ctx),
            attribute: inner.bool("attribute", ctx),
            wrapped: inner.bool("wrapped", ctx),
            extensions: BTreeMap::new(),
        };
        xml.extensions = inner.rest();
        xml
    });
    schema.external_docs = external_docs(node, "externalDocs", ctx);
}

/// Read an `externalDocs` member. Shared with the document model, which has four of them.
pub(crate) fn external_docs(node: &mut Members, key: &str, ctx: &mut Ctx) -> Option<ExternalDocs> {
    node.object(key, ctx, |mut inner, ctx| {
        let mut docs = ExternalDocs {
            description: inner.string("description", ctx),
            url: inner.string("url", ctx),
            extensions: BTreeMap::new(),
        };
        docs.extensions = inner.rest();
        docs
    })
}

fn types(node: &mut Members, ctx: &mut Ctx) -> Option<OneOrMany<TypeName>> {
    node.typed(
        "type",
        ctx,
        "a type name or an array of type names",
        |value, _| match value {
            Value::String(text) => Ok(OneOrMany::One(TypeName::parse(&text))),
            Value::Array(items) if items.iter().all(Value::is_string) => Ok(OneOrMany::Many(
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(TypeName::parse))
                    .collect(),
            )),
            other => Err(other),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{super::Schema, super::SchemaStore, super::TypeName, schema};
    use crate::diag::Ctx;

    fn parse(
        value: serde_json::Value,
    ) -> (
        SchemaStore,
        Box<super::SchemaObject>,
        Vec<crate::Diagnostic>,
    ) {
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        let id = schema(value, &mut store, &mut ctx).unwrap();
        let object = match store.get(id).clone() {
            Schema::Object(object) => object,
            Schema::Bool(_) => panic!("expected an object schema"),
        };
        (store, object, ctx.into_diagnostics())
    }

    #[test]
    fn booleans_are_schemas() {
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        let id = schema(json!(false), &mut store, &mut ctx).unwrap();
        assert_eq!(store.get(id), &Schema::Bool(false));
        assert!(ctx.into_diagnostics().is_empty());
    }

    #[test]
    fn a_scalar_is_not_a_schema_and_comes_back_to_the_caller() {
        let mut store = SchemaStore::default();
        let mut ctx = Ctx::new();
        assert_eq!(
            schema(json!("string"), &mut store, &mut ctx),
            Err(json!("string"))
        );
    }

    #[test]
    fn every_keyword_group_is_read() {
        let (_store, object, diagnostics) = parse(json!({
            "$id": "https://example.test/a",
            "$comment": "note",
            "allOf": [{"type": "object"}, true],
            "properties": {"a": {"type": "string"}},
            "additionalProperties": false,
            "prefixItems": [{"type": "string"}, {"type": "integer"}],
            "type": ["object", "null"],
            "required": ["a"],
            "minimum": 1,
            "title": "A",
            "format": "uuid",
            "discriminator": {"propertyName": "kind", "mapping": {"a": "#/x"}},
            "xml": {"name": "a", "wrapped": true},
            "externalDocs": {"url": "https://example.test"},
        }));
        assert!(diagnostics.is_empty());
        assert_eq!(object.id.as_deref(), Some("https://example.test/a"));
        assert_eq!(object.all_of.as_ref().map(Vec::len), Some(2));
        assert_eq!(object.properties.as_ref().map(BTreeMap::len), Some(1));
        assert!(object.additional_properties.is_some());
        assert_eq!(object.prefix_items.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            object
                .types
                .as_ref()
                .map(|t| t.iter().cloned().collect::<Vec<_>>()),
            Some(vec![TypeName::Object, TypeName::Null])
        );
        assert_eq!(object.required.as_deref(), Some(&["a".to_owned()][..]));
        assert_eq!(
            object.minimum.as_ref().map(ToString::to_string).as_deref(),
            Some("1")
        );
        assert_eq!(object.format.as_deref(), Some("uuid"));
        assert_eq!(
            object
                .discriminator
                .as_ref()
                .and_then(|d| d.property_name.as_deref()),
            Some("kind")
        );
        assert_eq!(object.xml.as_ref().and_then(|x| x.wrapped), Some(true));
        assert!(object.external_docs.is_some());
        assert!(object.unknown.is_empty());
    }

    #[test]
    fn unmodelled_keywords_are_kept_verbatim() {
        let (_store, object, diagnostics) = parse(json!({
            "type": "string",
            "x-vendor": {"deep": [1, 2]},
            "futureKeyword": "whatever",
        }));
        assert!(diagnostics.is_empty());
        assert_eq!(object.unknown.len(), 2);
        assert_eq!(object.unknown["x-vendor"], json!({"deep": [1, 2]}));
    }

    #[test]
    fn a_reference_with_siblings_is_stored_as_written() {
        // 2020-12 applies sibling keywords to the reference; interpreting that is the shape
        // layer's decision, so the model must not pre-chew it.
        let (_store, object, diagnostics) = parse(json!({
            "$ref": "#/components/schemas/Pet",
            "description": "one pet",
        }));
        assert!(diagnostics.is_empty());
        assert_eq!(
            object.reference.as_deref(),
            Some("#/components/schemas/Pet")
        );
        assert_eq!(object.description.as_deref(), Some("one pet"));
    }

    #[test]
    fn a_malformed_keyword_is_kept_and_diagnosed() {
        let (_store, object, diagnostics) = parse(json!({"required": "a", "type": "object"}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location().to_string(), "/required");
        assert!(object.required.is_none());
        assert_eq!(object.unknown["required"], json!("a"));
    }

    #[test]
    fn one_bad_subschema_preserves_the_whole_applicator() {
        let (_store, object, diagnostics) = parse(json!({"anyOf": [{"type": "string"}, 7]}));
        assert_eq!(diagnostics.len(), 1);
        assert!(object.any_of.is_none());
        assert_eq!(object.unknown["anyOf"], json!([{"type": "string"}, 7]));
    }

    #[test]
    fn recursion_is_just_a_deep_store() {
        let (store, object, _) = parse(json!({
            "type": "object",
            "properties": {"child": {"$ref": "#/x", "properties": {"leaf": true}}},
        }));
        assert!(object.properties.is_some());
        assert_eq!(store.len(), 3);
    }
}
