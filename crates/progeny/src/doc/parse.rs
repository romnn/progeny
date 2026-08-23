//! Reading the document tree out of a normalized JSON value.
//!
//! Total by construction: every node reader consumes the members it understands and hands the
//! rest to `extensions`. Nothing here returns an error — a document that reached this point has
//! already been accepted, and a malformed member below the root is a diagnostic, not a
//! rejection.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    Callback, Components, Contact, Document, Encoding, Example, Header, Info, License, Link,
    MaybeRef, MediaType, OAuthFlow, OAuthFlows, Operation, Parameter, ParsedDocument, PathItem,
    Paths, Reference, RequestBody, Response, Responses, SecurityRequirement, SecurityScheme,
    Server, ServerVariable, Tag,
};
use crate::diag::Ctx;
use crate::normalize::Normalized;
use crate::schema::{SchemaStore, parse as schema_parse};
use crate::value::Members;

/// Parse a normalized document.
///
/// Takes [`Normalized`] rather than a bare value, which is how "3.0 never reaches the parser" is
/// enforced: there is exactly one parser, and the only way to reach it is through dialect
/// detection and rewriting.
pub(crate) fn document(normalized: Normalized, ctx: &mut Ctx) -> ParsedDocument {
    let mut store = SchemaStore::default();
    let dialect_30 = normalized.dialect_30();
    let root = match normalized.into_value() {
        Value::Object(map) => map,
        // Unreachable: normalization rejects a root that is not an object.
        _ => serde_json::Map::new(),
    };
    let mut node = Members::new(root);

    let mut document =
        Document {
            openapi: node.string("openapi", ctx),
            info: node.object("info", ctx, info),
            json_schema_dialect: node.string("jsonSchemaDialect", ctx),
            servers: node.object_array("servers", ctx, "an array of servers", server),
            paths: node.typed("paths", ctx, "a paths object", |value, ctx| match value {
                Value::Object(map) => Ok(paths(Members::new(map), ctx, &mut store)),
                other => Err(other),
            }),
            webhooks: maybe_ref_map(&mut node, "webhooks", ctx, &mut store, path_item),
            components: node.typed("components", ctx, "a components object", |value, ctx| {
                match value {
                    Value::Object(map) => Ok(components(Members::new(map), ctx, &mut store)),
                    other => Err(other),
                }
            }),
            security: security(&mut node, ctx),
            tags: node.object_array("tags", ctx, "an array of tags", tag),
            external_docs: schema_parse::external_docs(&mut node, "externalDocs", ctx),
            extensions: BTreeMap::new(),
        };
    document.extensions = node.rest();

    ParsedDocument {
        document,
        schemas: store,
        dialect_30,
    }
}

/// Read a node that may be a Reference Object instead.
///
/// A `$ref` member is what makes it one. Everything else a reference carries beyond `summary`
/// and `description` is preserved in its extensions, so a reference with stray members — which
/// the specification says to ignore — still round-trips.
fn maybe_ref<T>(
    mut node: Members,
    ctx: &mut Ctx,
    read: impl FnOnce(Members, &mut Ctx) -> T,
) -> MaybeRef<T> {
    if !node.holds_string("$ref") {
        return MaybeRef::Item(read(node, ctx));
    }
    let mut reference = Reference {
        target: node.string("$ref", ctx),
        summary: node.string("summary", ctx),
        description: node.string("description", ctx),
        extensions: BTreeMap::new(),
    };
    reference.extensions = node.rest();
    MaybeRef::Reference(reference)
}

/// Read a member holding a name-to-node map where each node may be a reference.
fn maybe_ref_map<T>(
    node: &mut Members,
    key: &str,
    ctx: &mut Ctx,
    store: &mut SchemaStore,
    read: impl Fn(Members, &mut Ctx, &mut SchemaStore) -> T + Copy,
) -> Option<BTreeMap<String, MaybeRef<T>>> {
    node.object_map(
        key,
        ctx,
        "an object whose members are objects",
        |inner, ctx| maybe_ref(inner, ctx, |inner, ctx| read(inner, ctx, store)),
    )
}

/// Read a member holding an array of nodes where each node may be a reference.
fn maybe_ref_array<T>(
    node: &mut Members,
    key: &str,
    ctx: &mut Ctx,
    store: &mut SchemaStore,
    read: impl Fn(Members, &mut Ctx, &mut SchemaStore) -> T + Copy,
) -> Option<Vec<MaybeRef<T>>> {
    node.object_array(key, ctx, "an array of objects", |inner, ctx| {
        maybe_ref(inner, ctx, |inner, ctx| read(inner, ctx, store))
    })
}

fn info(mut node: Members, ctx: &mut Ctx) -> Info {
    let mut info = Info {
        title: node.string("title", ctx),
        summary: node.string("summary", ctx),
        description: node.string("description", ctx),
        terms_of_service: node.string("termsOfService", ctx),
        contact: node.object("contact", ctx, |mut inner, ctx| {
            let mut contact = Contact {
                name: inner.string("name", ctx),
                url: inner.string("url", ctx),
                email: inner.string("email", ctx),
                extensions: BTreeMap::new(),
            };
            contact.extensions = inner.rest();
            contact
        }),
        license: node.object("license", ctx, |mut inner, ctx| {
            let mut license = License {
                name: inner.string("name", ctx),
                identifier: inner.string("identifier", ctx),
                url: inner.string("url", ctx),
                extensions: BTreeMap::new(),
            };
            license.extensions = inner.rest();
            license
        }),
        version: node.string("version", ctx),
        extensions: BTreeMap::new(),
    };
    info.extensions = node.rest();
    info
}

fn server(mut node: Members, ctx: &mut Ctx) -> Server {
    let mut server = Server {
        url: node.string("url", ctx),
        description: node.string("description", ctx),
        variables: node.object_map(
            "variables",
            ctx,
            "an object of server variables",
            |mut inner, ctx| {
                let mut variable = ServerVariable {
                    enumeration: inner.string_array("enum", ctx),
                    default: inner.string("default", ctx),
                    description: inner.string("description", ctx),
                    extensions: BTreeMap::new(),
                };
                variable.extensions = inner.rest();
                variable
            },
        ),
        extensions: BTreeMap::new(),
    };
    server.extensions = node.rest();
    server
}

fn tag(mut node: Members, ctx: &mut Ctx) -> Tag {
    let mut tag = Tag {
        name: node.string("name", ctx),
        description: node.string("description", ctx),
        external_docs: schema_parse::external_docs(&mut node, "externalDocs", ctx),
        extensions: BTreeMap::new(),
    };
    tag.extensions = node.rest();
    tag
}

fn security(node: &mut Members, ctx: &mut Ctx) -> Option<Vec<SecurityRequirement>> {
    node.typed(
        "security",
        ctx,
        "an array of security requirements",
        |value, _| match value {
            Value::Array(items) if items.iter().all(is_security_requirement) => Ok(items
                .iter()
                .map(|item| {
                    item.as_object()
                        .into_iter()
                        .flatten()
                        .map(|(name, scopes)| {
                            let scopes = scopes
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|scope| scope.as_str().map(ToOwned::to_owned))
                                .collect();
                            (name.clone(), scopes)
                        })
                        .collect()
                })
                .collect()),
            other => Err(other),
        },
    )
}

fn is_security_requirement(value: &Value) -> bool {
    value.as_object().is_some_and(|entries| {
        entries.values().all(|scopes| {
            scopes
                .as_array()
                .is_some_and(|scopes| scopes.iter().all(Value::is_string))
        })
    })
}

/// Read the Paths Object.
///
/// A path template starts with `/`, which is what tells a path apart from an `x-*` member or a
/// typo. Anything else stays in `extensions`, so one malformed entry costs one entry rather than
/// the whole document's operations.
fn paths(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Paths {
    let mut items = BTreeMap::new();
    for key in node.keys() {
        if !key.starts_with('/') {
            continue;
        }
        if let Some(item) = node.object(&key, ctx, |inner, ctx| {
            maybe_ref(inner, ctx, |inner, ctx| path_item(inner, ctx, store))
        }) {
            items.insert(key, item);
        }
    }
    Paths {
        items,
        extensions: node.rest(),
    }
}

fn path_item(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> PathItem {
    let mut item = PathItem {
        summary: node.string("summary", ctx),
        description: node.string("description", ctx),
        get: operation_member(&mut node, "get", ctx, store),
        put: operation_member(&mut node, "put", ctx, store),
        post: operation_member(&mut node, "post", ctx, store),
        delete: operation_member(&mut node, "delete", ctx, store),
        options: operation_member(&mut node, "options", ctx, store),
        head: operation_member(&mut node, "head", ctx, store),
        patch: operation_member(&mut node, "patch", ctx, store),
        trace: operation_member(&mut node, "trace", ctx, store),
        servers: node.object_array("servers", ctx, "an array of servers", server),
        parameters: maybe_ref_array(&mut node, "parameters", ctx, store, parameter),
        extensions: BTreeMap::new(),
    };
    item.extensions = node.rest();
    item
}

fn operation_member(
    node: &mut Members,
    key: &str,
    ctx: &mut Ctx,
    store: &mut SchemaStore,
) -> Option<Operation> {
    node.object(key, ctx, |inner, ctx| operation(inner, ctx, store))
}

fn operation(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Operation {
    let mut operation = Operation {
        tags: node.string_array("tags", ctx),
        summary: node.string("summary", ctx),
        description: node.string("description", ctx),
        external_docs: schema_parse::external_docs(&mut node, "externalDocs", ctx),
        id: node.string("operationId", ctx),
        parameters: maybe_ref_array(&mut node, "parameters", ctx, store, parameter),
        request_body: node.object("requestBody", ctx, |inner, ctx| {
            maybe_ref(inner, ctx, |inner, ctx| request_body(inner, ctx, store))
        }),
        responses: node.typed(
            "responses",
            ctx,
            "a responses object",
            |value, ctx| match value {
                Value::Object(map) => Ok(responses(Members::new(map), ctx, store)),
                other => Err(other),
            },
        ),
        callbacks: maybe_ref_map(&mut node, "callbacks", ctx, store, callback),
        deprecated: node.bool("deprecated", ctx),
        security: security(&mut node, ctx),
        servers: node.object_array("servers", ctx, "an array of servers", server),
        extensions: BTreeMap::new(),
    };
    operation.extensions = node.rest();
    operation
}

fn parameter(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Parameter {
    let mut parameter = Parameter {
        name: node.string("name", ctx),
        location: node.string("in", ctx),
        description: node.string("description", ctx),
        required: node.bool("required", ctx),
        deprecated: node.bool("deprecated", ctx),
        allow_empty_value: node.bool("allowEmptyValue", ctx),
        style: node.string("style", ctx),
        explode: node.bool("explode", ctx),
        allow_reserved: node.bool("allowReserved", ctx),
        schema: schema_parse::member(&mut node, "schema", store, ctx),
        example: node.value("example"),
        examples: maybe_ref_map(&mut node, "examples", ctx, store, |inner, ctx, _| {
            example(inner, ctx)
        }),
        content: content(&mut node, ctx, store),
        extensions: BTreeMap::new(),
    };
    parameter.extensions = node.rest();
    parameter
}

fn header(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Header {
    let mut header = Header {
        description: node.string("description", ctx),
        required: node.bool("required", ctx),
        deprecated: node.bool("deprecated", ctx),
        allow_empty_value: node.bool("allowEmptyValue", ctx),
        style: node.string("style", ctx),
        explode: node.bool("explode", ctx),
        allow_reserved: node.bool("allowReserved", ctx),
        schema: schema_parse::member(&mut node, "schema", store, ctx),
        example: node.value("example"),
        examples: maybe_ref_map(&mut node, "examples", ctx, store, |inner, ctx, _| {
            example(inner, ctx)
        }),
        content: content(&mut node, ctx, store),
        extensions: BTreeMap::new(),
    };
    header.extensions = node.rest();
    header
}

fn request_body(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> RequestBody {
    let mut body = RequestBody {
        description: node.string("description", ctx),
        content: content(&mut node, ctx, store),
        required: node.bool("required", ctx),
        extensions: BTreeMap::new(),
    };
    body.extensions = node.rest();
    body
}

fn content(
    node: &mut Members,
    ctx: &mut Ctx,
    store: &mut SchemaStore,
) -> Option<BTreeMap<String, MediaType>> {
    node.object_map("content", ctx, "an object of media types", |inner, ctx| {
        media_type(inner, ctx, store)
    })
}

fn media_type(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> MediaType {
    let mut media_type = MediaType {
        schema: schema_parse::member(&mut node, "schema", store, ctx),
        example: node.value("example"),
        examples: maybe_ref_map(&mut node, "examples", ctx, store, |inner, ctx, _| {
            example(inner, ctx)
        }),
        encoding: node.object_map("encoding", ctx, "an object of encodings", |inner, ctx| {
            encoding(inner, ctx, store)
        }),
        extensions: BTreeMap::new(),
    };
    media_type.extensions = node.rest();
    media_type
}

fn encoding(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Encoding {
    let mut encoding = Encoding {
        content_type: node.string("contentType", ctx),
        headers: maybe_ref_map(&mut node, "headers", ctx, store, header),
        style: node.string("style", ctx),
        explode: node.bool("explode", ctx),
        allow_reserved: node.bool("allowReserved", ctx),
        extensions: BTreeMap::new(),
    };
    encoding.extensions = node.rest();
    encoding
}

/// Read the Responses Object.
///
/// Any member that is an object and is not `default` or an `x-*` extension is a status entry,
/// whatever it is spelled like: whether `2xx` or `200 ` or `success` means anything is the API
/// model's judgement to make and to diagnose, not the parser's.
fn responses(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Responses {
    let default = node.object("default", ctx, |inner, ctx| {
        maybe_ref(inner, ctx, |inner, ctx| response(inner, ctx, store))
    });
    let mut statuses = BTreeMap::new();
    for key in node.keys() {
        if key.starts_with("x-") || !node.holds_object(&key) {
            continue;
        }
        if let Some(entry) = node.object(&key, ctx, |inner, ctx| {
            maybe_ref(inner, ctx, |inner, ctx| response(inner, ctx, store))
        }) {
            statuses.insert(key, entry);
        }
    }
    Responses {
        default,
        statuses,
        extensions: node.rest(),
    }
}

fn response(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Response {
    let mut response = Response {
        description: node.string("description", ctx),
        headers: maybe_ref_map(&mut node, "headers", ctx, store, header),
        content: content(&mut node, ctx, store),
        links: maybe_ref_map(&mut node, "links", ctx, store, |inner, ctx, _| {
            link(inner, ctx)
        }),
        extensions: BTreeMap::new(),
    };
    response.extensions = node.rest();
    response
}

/// Read a Callback Object: every member that is an object is a runtime expression.
fn callback(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Callback {
    let mut expressions = BTreeMap::new();
    for key in node.keys() {
        if key.starts_with("x-") || !node.holds_object(&key) {
            continue;
        }
        if let Some(entry) = node.object(&key, ctx, |inner, ctx| {
            maybe_ref(inner, ctx, |inner, ctx| path_item(inner, ctx, store))
        }) {
            expressions.insert(key, entry);
        }
    }
    Callback {
        expressions,
        extensions: node.rest(),
    }
}

fn example(mut node: Members, ctx: &mut Ctx) -> Example {
    let mut example = Example {
        summary: node.string("summary", ctx),
        description: node.string("description", ctx),
        value: node.value("value"),
        external_value: node.string("externalValue", ctx),
        extensions: BTreeMap::new(),
    };
    example.extensions = node.rest();
    example
}

fn link(mut node: Members, ctx: &mut Ctx) -> Link {
    let mut link = Link {
        operation_ref: node.string("operationRef", ctx),
        operation_id: node.string("operationId", ctx),
        parameters: node.value_map("parameters", ctx, "an object"),
        request_body: node.value("requestBody"),
        description: node.string("description", ctx),
        server: node.object("server", ctx, server),
        extensions: BTreeMap::new(),
    };
    link.extensions = node.rest();
    link
}

fn security_scheme(mut node: Members, ctx: &mut Ctx) -> SecurityScheme {
    let mut scheme = SecurityScheme {
        kind: node.string("type", ctx),
        description: node.string("description", ctx),
        name: node.string("name", ctx),
        location: node.string("in", ctx),
        scheme: node.string("scheme", ctx),
        bearer_format: node.string("bearerFormat", ctx),
        flows: node.object("flows", ctx, |mut inner, ctx| {
            let mut flows = OAuthFlows {
                implicit: oauth_flow(&mut inner, "implicit", ctx),
                password: oauth_flow(&mut inner, "password", ctx),
                client_credentials: oauth_flow(&mut inner, "clientCredentials", ctx),
                authorization_code: oauth_flow(&mut inner, "authorizationCode", ctx),
                extensions: BTreeMap::new(),
            };
            flows.extensions = inner.rest();
            flows
        }),
        open_id_connect_url: node.string("openIdConnectUrl", ctx),
        extensions: BTreeMap::new(),
    };
    scheme.extensions = node.rest();
    scheme
}

fn oauth_flow(node: &mut Members, key: &str, ctx: &mut Ctx) -> Option<OAuthFlow> {
    node.object(key, ctx, |mut inner, ctx| {
        let mut flow = OAuthFlow {
            authorization_url: inner.string("authorizationUrl", ctx),
            token_url: inner.string("tokenUrl", ctx),
            refresh_url: inner.string("refreshUrl", ctx),
            scopes: inner.string_map("scopes", ctx, "an object of strings"),
            extensions: BTreeMap::new(),
        };
        flow.extensions = inner.rest();
        flow
    })
}

fn components(mut node: Members, ctx: &mut Ctx, store: &mut SchemaStore) -> Components {
    let mut components = Components {
        schemas: schema_parse::member_map(&mut node, "schemas", store, ctx),
        responses: maybe_ref_map(&mut node, "responses", ctx, store, response),
        parameters: maybe_ref_map(&mut node, "parameters", ctx, store, parameter),
        examples: maybe_ref_map(&mut node, "examples", ctx, store, |inner, ctx, _| {
            example(inner, ctx)
        }),
        request_bodies: maybe_ref_map(&mut node, "requestBodies", ctx, store, request_body),
        headers: maybe_ref_map(&mut node, "headers", ctx, store, header),
        security_schemes: maybe_ref_map(
            &mut node,
            "securitySchemes",
            ctx,
            store,
            |inner, ctx, _| security_scheme(inner, ctx),
        ),
        links: maybe_ref_map(&mut node, "links", ctx, store, |inner, ctx, _| {
            link(inner, ctx)
        }),
        callbacks: maybe_ref_map(&mut node, "callbacks", ctx, store, callback),
        path_items: maybe_ref_map(&mut node, "pathItems", ctx, store, path_item),
        extensions: BTreeMap::new(),
    };
    components.extensions = node.rest();
    components
}
