//! Dialect normalization: OpenAPI 3.0 documents rewritten into 3.1 form, before parsing.
//!
//! 3.0's schema dialect is a modified draft-05 subset and 3.1's is JSON Schema 2020-12. The two
//! converge on **one lowering** by rewriting 3.0 into 3.1 form here, as a pure `Value → Value`
//! function: independently testable, and structurally incapable of the lossy mid-pipeline
//! conversion that a second parser would invite.
//!
//! The walk is **positional**: it descends through the places a 3.0 document keeps schemas
//! rather than rewriting any object that happens to contain a `nullable` member. That
//! distinction is load-bearing. A pattern-matching rewrite would corrupt example payloads and
//! vendor extensions that contain such keys innocently — `nullable`, `example` and `format` are
//! all ordinary words that appear inside `example:` blocks all over the corpus.

use serde_json::{Map, Value};

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};

/// A document in 3.1 form.
///
/// Constructible only by [`normalize`], which is how "3.0 never reaches the parser" is a
/// property of the type system rather than a convention: the parse entry point takes this, and
/// the only way to obtain one is to have gone through version detection and rewriting.
#[derive(Debug)]
pub(crate) struct Normalized {
    value: Value,
    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read by the corpus harness, which is feature-gated"
        )
    )]
    version: Version,
}

impl Normalized {
    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read by the corpus harness, which is feature-gated"
        )
    )]
    pub(crate) fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }

    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read by the corpus harness, which is feature-gated"
        )
    )]
    pub(crate) fn version(&self) -> &Version {
        &self.version
    }
}

/// The `openapi` version a document declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    /// Exactly what the document wrote, kept because it is part of the document.
    pub(crate) text: String,
}

/// Detect the dialect and rewrite it into 3.1 form.
pub(crate) fn normalize(value: Value, ctx: &mut Ctx) -> Result<Normalized, RejectError> {
    let Value::Object(mut root) = value else {
        return Err(RejectError::new(
            RejectKind::NotAnObject,
            "an OpenAPI document is a JSON object; this document's root is not one",
        ));
    };

    let version = version(&root)?;
    if version.major != 3 {
        return Err(RejectError::new(
            RejectKind::UnsupportedVersion,
            format!(
                "`openapi: {}` declares major version {}; progeny implements OpenAPI 3",
                version.text, version.major
            ),
        )
        .at(JsonPointer::root().child("openapi")));
    }
    if !root.contains_key("paths") && !root.contains_key("webhooks") {
        return Err(RejectError::new(
            RejectKind::NoOperations,
            "the document has neither `paths` nor `webhooks`, so it describes no operations",
        ));
    }

    match version.minor {
        0 => document(&mut root),
        1 => {}
        // A minor version from the future is read as 3.1 rather than rejected: 3.1 is the
        // dialect the parser implements, the model is a superset that holds members it does not
        // interpret, and a document is more useful read imperfectly than not at all.
        _ => ctx.report(Diagnostic::new(
            BreakageClass::UnsupportedDialect,
            Action::Warn,
            JsonPointer::root().child("openapi"),
            format!(
                "`openapi: {}` is newer than 3.1; read it as 3.1, so members this version adds \
                 are preserved but not interpreted",
                version.text
            ),
        )),
    }

    Ok(Normalized {
        value: Value::Object(root),
        version,
    })
}

fn version(root: &Map<String, Value>) -> Result<Version, RejectError> {
    let Some(declared) = root.get("openapi") else {
        return Err(RejectError::new(
            RejectKind::MissingVersion,
            "the document has no `openapi` member, so its version is unknown",
        ));
    };
    // YAML resolves an unquoted `3.0` to a number, so the member is a number in real documents
    // as often as it deserves to be a string.
    let text = match declared {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => {
            return Err(RejectError::new(
                RejectKind::MissingVersion,
                format!("`openapi` should be a version string; it is {other}"),
            )
            .at(JsonPointer::root().child("openapi")));
        }
    };

    let mut components = text.trim().split('.');
    let major = components.next().and_then(|part| part.parse::<u32>().ok());
    let minor = components
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let Some(major) = major else {
        return Err(RejectError::new(
            RejectKind::MissingVersion,
            format!("`openapi: {text}` is not a version number"),
        )
        .at(JsonPointer::root().child("openapi")));
    };

    Ok(Version { major, minor, text })
}

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Every place a subschema can appear under exactly one key.
const SUBSCHEMA: [&str; 11] = [
    "additionalProperties",
    "contains",
    "contentSchema",
    "else",
    "if",
    "items",
    "not",
    "propertyNames",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
];

/// Every place an array of subschemas can appear.
const SUBSCHEMA_ARRAY: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// Every place a name-to-subschema map can appear. `definitions` is not an OpenAPI keyword, but
/// tools that lower JSON Schema into 3.0 emit it, and descending into it costs nothing.
const SUBSCHEMA_MAP: [&str; 5] = [
    "$defs",
    "definitions",
    "dependentSchemas",
    "patternProperties",
    "properties",
];

fn document(root: &mut Map<String, Value>) {
    if let Some(Value::Object(paths)) = root.get_mut("paths") {
        for (key, item) in paths.iter_mut() {
            if !is_extension(key) {
                path_item(item);
            }
        }
    }
    if let Some(Value::Object(webhooks)) = root.get_mut("webhooks") {
        for (key, item) in webhooks.iter_mut() {
            if !is_extension(key) {
                path_item(item);
            }
        }
    }
    if let Some(Value::Object(components)) = root.get_mut("components") {
        for (key, entries) in components.iter_mut() {
            let Value::Object(entries) = entries else {
                continue;
            };
            let visit: fn(&mut Value) = match key.as_str() {
                "schemas" => schema,
                "responses" => response,
                "parameters" | "headers" => parameter,
                "requestBodies" => request_body,
                "callbacks" => callback,
                "pathItems" => path_item,
                // `examples`, `links` and `securitySchemes` hold no schemas.
                _ => continue,
            };
            for entry in entries.values_mut() {
                visit(entry);
            }
        }
    }
}

fn path_item(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(Value::Array(parameters)) = map.get_mut("parameters") {
        for entry in parameters {
            parameter(entry);
        }
    }
    for method in METHODS {
        if let Some(entry) = map.get_mut(method) {
            operation(entry);
        }
    }
}

fn operation(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(Value::Array(parameters)) = map.get_mut("parameters") {
        for entry in parameters {
            parameter(entry);
        }
    }
    if let Some(entry) = map.get_mut("requestBody") {
        request_body(entry);
    }
    if let Some(Value::Object(responses)) = map.get_mut("responses") {
        for (key, entry) in responses.iter_mut() {
            if !is_extension(key) {
                response(entry);
            }
        }
    }
    if let Some(Value::Object(callbacks)) = map.get_mut("callbacks") {
        for (key, entry) in callbacks.iter_mut() {
            if !is_extension(key) {
                callback(entry);
            }
        }
    }
}

fn callback(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, entry) in map.iter_mut() {
        if !is_extension(key) {
            path_item(entry);
        }
    }
}

/// Parameters and headers are the same node bar the `name` and `in` members.
fn parameter(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(entry) = map.get_mut("schema") {
        schema(entry);
    }
    content(map);
}

fn request_body(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    content(map);
}

fn response(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(Value::Object(headers)) = map.get_mut("headers") {
        for entry in headers.values_mut() {
            parameter(entry);
        }
    }
    content(map);
}

fn content(map: &mut Map<String, Value>) {
    let Some(Value::Object(media_types)) = map.get_mut("content") else {
        return;
    };
    for entry in media_types.values_mut() {
        media_type(entry);
    }
}

fn media_type(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(entry) = map.get_mut("schema") {
        schema(entry);
    }
    if let Some(Value::Object(encodings)) = map.get_mut("encoding") {
        for entry in encodings.values_mut() {
            let Value::Object(encoding) = entry else {
                continue;
            };
            if let Some(Value::Object(headers)) = encoding.get_mut("headers") {
                for header in headers.values_mut() {
                    parameter(header);
                }
            }
        }
    }
}

/// Rewrite one schema and descend into its subschemas.
///
/// Boolean schemas and anything that is not an object have nothing to rewrite.
fn schema(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    nullable(map);
    exclusive_bounds(map);
    example(map);
    string_format(map);

    for key in SUBSCHEMA {
        if let Some(entry) = map.get_mut(key) {
            schema(entry);
        }
    }
    for key in SUBSCHEMA_ARRAY {
        if let Some(Value::Array(entries)) = map.get_mut(key) {
            for entry in entries {
                schema(entry);
            }
        }
    }
    for key in SUBSCHEMA_MAP {
        if let Some(Value::Object(entries)) = map.get_mut(key) {
            for entry in entries.values_mut() {
                schema(entry);
            }
        }
    }
}

/// `nullable: true` becomes `"null"` in the type — and in the enum.
///
/// Adding `"null"` to `type` alone would be a silent narrowing wherever an enum is also present:
/// in 3.1 an instance must satisfy both, so `type: [string, "null"]` with `enum: [a, b]` rejects
/// the very `null` the document was asking to allow.
fn nullable(map: &mut Map<String, Value>) {
    match map.get("nullable") {
        Some(Value::Bool(true)) => {}
        // `nullable: false` is 3.1's default and says nothing there.
        Some(Value::Bool(false)) => {
            map.remove("nullable");
            return;
        }
        // Absent, or a value that is not a boolean: leave it for the parser to preserve and
        // diagnose rather than guessing what it meant.
        _ => return,
    }
    map.remove("nullable");

    match map.get_mut("type") {
        Some(Value::String(name)) => {
            let name = std::mem::take(name);
            map.insert(
                "type".to_owned(),
                Value::Array(vec![Value::String(name), Value::String("null".to_owned())]),
            );
        }
        Some(Value::Array(names)) if !names.iter().any(|name| name == "null") => {
            names.push(Value::String("null".to_owned()));
        }
        // With no `type`, nothing narrows the instance to a non-null value, so there is
        // nothing to widen.
        _ => {}
    }

    if let Some(Value::Array(values)) = map.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
}

/// 3.0's boolean `exclusiveMinimum`/`exclusiveMaximum` modify a sibling bound; 3.1's are the
/// bound.
fn exclusive_bounds(map: &mut Map<String, Value>) {
    for (flag, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        match map.get(flag) {
            Some(Value::Bool(true)) => {
                map.remove(flag);
                if let Some(value) = map.remove(bound) {
                    map.insert(flag.to_owned(), value);
                }
            }
            // `exclusive*: false` means the sibling bound is inclusive, which is what a bare
            // bound already means.
            Some(Value::Bool(false)) => {
                map.remove(flag);
            }
            _ => {}
        }
    }
}

/// 3.0's schema-level `example` is 3.1's `examples`, which is an array.
fn example(map: &mut Map<String, Value>) {
    if map.contains_key("examples") {
        return;
    }
    if let Some(value) = map.remove("example") {
        map.insert("examples".to_owned(), Value::Array(vec![value]));
    }
}

/// 3.0 says "this string is base64" and "this string is bytes" with `format`; 3.1 says the first
/// with `contentEncoding` and the second at the media-type level, which is where 3.0 and 3.1
/// genuinely differ about *where* the fact lives.
fn string_format(map: &mut Map<String, Value>) {
    if !type_includes(map, "string") {
        return;
    }
    match map.get("format").and_then(Value::as_str) {
        Some("byte") => {
            map.remove("format");
            map.entry("contentEncoding")
                .or_insert_with(|| Value::String("base64".to_owned()));
        }
        // 3.1's replacement for `format: binary`. Not simply dropped: a
        // multipart body marks *which property* is a file this way, and the
        // media-type key cannot carry a per-property fact.
        Some("binary") => {
            map.remove("format");
            map.entry("contentMediaType")
                .or_insert_with(|| Value::String("application/octet-stream".to_owned()));
        }
        _ => {}
    }
}

fn type_includes(map: &Map<String, Value>, name: &str) -> bool {
    match map.get("type") {
        Some(Value::String(declared)) => declared == name,
        Some(Value::Array(declared)) => declared.iter().any(|entry| entry == name),
        _ => false,
    }
}

fn is_extension(key: &str) -> bool {
    key.starts_with("x-")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::normalize;
    use crate::diag::{Action, BreakageClass, Ctx, RejectKind};

    fn normalized(value: Value) -> Value {
        let mut ctx = Ctx::new();
        normalize(value, &mut ctx).unwrap().into_value()
    }

    /// A 3.0 document with one schema at `components.schemas.S`.
    fn with_schema(schema: &Value) -> Value {
        json!({"openapi": "3.0.3", "paths": {}, "components": {"schemas": {"S": schema}}})
    }

    fn normalized_schema(schema: &Value) -> Value {
        normalized(with_schema(schema))["components"]["schemas"]["S"].clone()
    }

    #[test]
    fn a_31_document_is_left_alone() {
        let original = json!({
            "openapi": "3.1.0",
            "paths": {},
            "components": {"schemas": {"S": {"type": ["string", "null"], "example": 1}}},
        });
        assert_eq!(normalized(original.clone()), original);
    }

    #[test]
    fn nullable_becomes_a_null_type_in_array_form() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": true})),
            json!({"type": ["string", "null"]})
        );
        assert_eq!(
            normalized_schema(&json!({"type": ["string"], "nullable": true})),
            json!({"type": ["string", "null"]})
        );
        assert_eq!(
            normalized_schema(&json!({"type": ["string", "null"], "nullable": true})),
            json!({"type": ["string", "null"]})
        );
    }

    #[test]
    fn nullable_also_widens_an_enum() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "enum": ["a", "b"], "nullable": true})),
            json!({"type": ["string", "null"], "enum": ["a", "b", null]})
        );
    }

    #[test]
    fn nullable_without_a_type_narrows_nothing() {
        assert_eq!(
            normalized_schema(&json!({"nullable": true, "description": "d"})),
            json!({"description": "d"})
        );
    }

    #[test]
    fn nullable_false_is_the_31_default() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": false})),
            json!({"type": "string"})
        );
    }

    #[test]
    fn a_non_boolean_nullable_is_left_for_the_parser_to_diagnose() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": "yes"})),
            json!({"type": "string", "nullable": "yes"})
        );
    }

    #[test]
    fn boolean_exclusive_bounds_become_numeric_ones() {
        assert_eq!(
            normalized_schema(&json!({"minimum": 0, "exclusiveMinimum": true})),
            json!({"exclusiveMinimum": 0})
        );
        assert_eq!(
            normalized_schema(&json!({"maximum": 10, "exclusiveMaximum": false})),
            json!({"maximum": 10})
        );
        // A flag with nothing to modify is just noise.
        assert_eq!(
            normalized_schema(&json!({"exclusiveMinimum": true})),
            json!({})
        );
    }

    #[test]
    fn schema_level_example_becomes_examples() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "example": "a"})),
            json!({"type": "string", "examples": ["a"]})
        );
        // An already-3.1 `examples` wins; the stray `example` is left for the parser to keep.
        assert_eq!(
            normalized_schema(&json!({"examples": ["a"], "example": "b"})),
            json!({"examples": ["a"], "example": "b"})
        );
    }

    #[test]
    fn string_formats_move_to_where_31_says_them() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "byte"})),
            json!({"type": "string", "contentEncoding": "base64"})
        );
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "binary"})),
            json!({"type": "string", "contentMediaType": "application/octet-stream"})
        );
        // An explicit `contentMediaType` wins; normalization never overwrites
        // what the document already said.
        assert_eq!(
            normalized_schema(
                &json!({"type": "string", "format": "binary", "contentMediaType": "image/png"})
            ),
            json!({"type": "string", "contentMediaType": "image/png"})
        );
        // `format: binary` on something that is not a string is not the 3.0 idiom.
        assert_eq!(
            normalized_schema(&json!({"type": "object", "format": "binary"})),
            json!({"type": "object", "format": "binary"})
        );
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "date-time"})),
            json!({"type": "string", "format": "date-time"})
        );
    }

    #[test]
    fn rewrites_reach_every_subschema_position() {
        let rewritten = normalized_schema(&json!({
            "allOf": [{"type": "string", "nullable": true}],
            "properties": {"a": {"type": "integer", "nullable": true}},
            "items": {"type": "boolean", "nullable": true},
            "additionalProperties": {"type": "number", "nullable": true},
            "$defs": {"D": {"type": "string", "nullable": true}},
        }));
        assert_eq!(rewritten["allOf"][0]["type"], json!(["string", "null"]));
        assert_eq!(
            rewritten["properties"]["a"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(rewritten["items"]["type"], json!(["boolean", "null"]));
        assert_eq!(
            rewritten["additionalProperties"]["type"],
            json!(["number", "null"])
        );
        assert_eq!(rewritten["$defs"]["D"]["type"], json!(["string", "null"]));
    }

    #[test]
    fn rewrites_reach_schemas_under_paths_and_responses() {
        let document = json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "parameters": [{"name": "a", "in": "query", "schema": {"type": "string", "nullable": true}}],
                    "post": {
                        "requestBody": {"content": {"application/json": {"schema": {"type": "string", "nullable": true}}}},
                        "responses": {
                            "200": {
                                "description": "ok",
                                "headers": {"X-A": {"schema": {"type": "string", "nullable": true}}},
                                "content": {"application/json": {"schema": {"type": "integer", "nullable": true}}},
                            },
                        },
                    },
                },
            },
        });
        let out = normalized(document);
        let path = &out["paths"]["/pets"];
        assert_eq!(
            path["parameters"][0]["schema"]["type"],
            json!(["string", "null"])
        );
        let post = &path["post"];
        assert_eq!(
            post["requestBody"]["content"]["application/json"]["schema"]["type"],
            json!(["string", "null"])
        );
        let response = &post["responses"]["200"];
        assert_eq!(
            response["headers"]["X-A"]["schema"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            response["content"]["application/json"]["schema"]["type"],
            json!(["integer", "null"])
        );
    }

    #[test]
    fn decoys_outside_schema_positions_are_untouched() {
        // Every one of these is an ordinary word inside a payload, not a keyword. A rewrite
        // that pattern-matched on member names would corrupt all of them.
        let document = json!({
            "openapi": "3.0.1",
            "x-defaults": {"nullable": true, "type": "string", "example": "keep me"},
            "paths": {
                "/a": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "d",
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object"},
                                        "example": {"nullable": true, "type": "string", "format": "byte"},
                                        "examples": {
                                            "one": {"value": {"nullable": true, "minimum": 1, "exclusiveMinimum": true}},
                                        },
                                    },
                                },
                            },
                        },
                    },
                },
            },
            "components": {
                "schemas": {
                    "S": {
                        "type": "object",
                        "properties": {
                            "nullable": {"type": "boolean"},
                            "example": {"type": "string"},
                        },
                        "default": {"nullable": true, "example": 1},
                        "examples": [{"nullable": true, "format": "binary", "type": "string"}],
                    },
                },
            },
        });
        assert_eq!(normalized(document.clone()), document);
    }

    #[test]
    fn a_property_named_like_a_keyword_is_still_a_schema() {
        let rewritten = normalized_schema(&json!({
            "type": "object",
            "properties": {"nullable": {"type": "string", "nullable": true}},
        }));
        assert_eq!(
            rewritten["properties"]["nullable"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn a_yaml_number_version_is_a_version() {
        let value: Value = serde_json::from_str(r#"{"openapi": 3.0, "paths": {}}"#).unwrap();
        let mut ctx = Ctx::new();
        let out = normalize(value, &mut ctx).unwrap();
        assert_eq!(out.version().major, 3);
        assert_eq!(out.version().minor, 0);
        assert_eq!(out.version().text, "3.0");
    }

    #[test]
    fn the_declared_version_string_is_not_rewritten() {
        let out = normalized(json!({"openapi": "3.0.2", "paths": {}}));
        assert_eq!(out["openapi"], json!("3.0.2"));
    }

    #[test]
    fn a_future_minor_version_is_read_as_31_with_a_warning() {
        let mut ctx = Ctx::new();
        normalize(json!({"openapi": "3.2.0", "paths": {}}), &mut ctx).unwrap();
        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::UnsupportedDialect);
        assert_eq!(diagnostics[0].action(), Action::Warn);
    }

    #[test]
    fn documents_that_cannot_be_used_are_rejected() {
        let mut ctx = Ctx::new();
        for (value, kind) in [
            (json!([]), RejectKind::NotAnObject),
            (json!({"paths": {}}), RejectKind::MissingVersion),
            (
                json!({"openapi": "x", "paths": {}}),
                RejectKind::MissingVersion,
            ),
            (
                json!({"openapi": "2.0", "paths": {}}),
                RejectKind::UnsupportedVersion,
            ),
            (
                json!({"openapi": "4.0.0", "paths": {}}),
                RejectKind::UnsupportedVersion,
            ),
            (json!({"openapi": "3.1.0"}), RejectKind::NoOperations),
        ] {
            assert_eq!(
                normalize(value.clone(), &mut ctx).unwrap_err().kind(),
                kind,
                "{value}"
            );
        }
    }

    #[test]
    fn webhooks_alone_describe_operations() {
        let mut ctx = Ctx::new();
        assert!(normalize(json!({"openapi": "3.1.0", "webhooks": {}}), &mut ctx).is_ok());
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = normalized(with_schema(&json!({
            "type": "string",
            "nullable": true,
            "format": "byte",
            "example": "a",
            "minimum": 1,
            "exclusiveMinimum": true,
        })));
        let mut ctx = Ctx::new();
        let twice = normalize(once.clone(), &mut ctx).unwrap().into_value();
        assert_eq!(once, twice);
    }
}
