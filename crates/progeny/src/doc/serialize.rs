//! Writing the document tree back out as the JSON value it was read from.
//!
//! Mirrors [`super::parse`] node for node and member for member. The corpus round-trip is what
//! keeps the two halves honest.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    Callback, Components, Contact, Document, Encoding, Example, Header, Info, License, Link,
    MaybeRef, MediaType, OAuthFlow, OAuthFlows, Operation, Parameter, ParsedDocument, PathItem,
    Paths, Reference, RequestBody, Response, Responses, SecurityRequirement, SecurityScheme,
    Server, ServerVariable, Tag,
};
use crate::schema::{SchemaStore, serialize as schema_serialize};
use crate::value::Builder;

pub(crate) fn document(parsed: &ParsedDocument) -> Value {
    let store = &parsed.schemas;
    let document: &Document = &parsed.document;
    let mut out = Builder::new();
    out.set("openapi", document.openapi.clone());
    out.set_with("info", document.info.as_ref(), info_to_value);
    out.set("jsonSchemaDialect", document.json_schema_dialect.clone());
    out.set_array("servers", document.servers.as_deref(), server_to_value);
    out.set_with("paths", document.paths.as_ref(), |paths| {
        paths_to_value(store, paths)
    });
    out.set_with("webhooks", document.webhooks.as_ref(), |webhooks| {
        maybe_ref_map_to_value(webhooks, |item| path_item_to_value(store, item))
    });
    out.set_with("components", document.components.as_ref(), |components| {
        components_to_value(store, components)
    });
    out.set_array(
        "security",
        document.security.as_deref(),
        security_requirement_to_value,
    );
    out.set_array("tags", document.tags.as_deref(), tag_to_value);
    out.set_with(
        "externalDocs",
        document.external_docs.as_ref(),
        schema_serialize::external_docs_to_value,
    );
    out.extend(&document.extensions);
    out.finish()
}

fn reference_to_value(reference: &Reference) -> Value {
    let mut out = Builder::new();
    out.set("$ref", reference.target.clone());
    out.set("summary", reference.summary.clone());
    out.set("description", reference.description.clone());
    out.extend(&reference.extensions);
    out.finish()
}

fn maybe_ref_to_value<T>(node: &MaybeRef<T>, item: impl FnOnce(&T) -> Value) -> Value {
    match node {
        MaybeRef::Reference(reference) => reference_to_value(reference),
        MaybeRef::Item(inner) => item(inner),
    }
}

fn maybe_ref_map_to_value<T>(
    entries: &BTreeMap<String, MaybeRef<T>>,
    mut item: impl FnMut(&T) -> Value,
) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(name, node)| (name.clone(), maybe_ref_to_value(node, &mut item)))
            .collect(),
    )
}

fn maybe_ref_array_to_value<T>(
    entries: &[MaybeRef<T>],
    mut item: impl FnMut(&T) -> Value,
) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|node| maybe_ref_to_value(node, &mut item))
            .collect(),
    )
}

fn info_to_value(info: &Info) -> Value {
    let mut out = Builder::new();
    out.set("title", info.title.clone());
    out.set("summary", info.summary.clone());
    out.set("description", info.description.clone());
    out.set("termsOfService", info.terms_of_service.clone());
    out.set_with("contact", info.contact.as_ref(), contact_to_value);
    out.set_with("license", info.license.as_ref(), license_to_value);
    out.set("version", info.version.clone());
    out.extend(&info.extensions);
    out.finish()
}

fn contact_to_value(contact: &Contact) -> Value {
    let mut out = Builder::new();
    out.set("name", contact.name.clone());
    out.set("url", contact.url.clone());
    out.set("email", contact.email.clone());
    out.extend(&contact.extensions);
    out.finish()
}

fn license_to_value(license: &License) -> Value {
    let mut out = Builder::new();
    out.set("name", license.name.clone());
    out.set("identifier", license.identifier.clone());
    out.set("url", license.url.clone());
    out.extend(&license.extensions);
    out.finish()
}

fn server_to_value(server: &Server) -> Value {
    let mut out = Builder::new();
    out.set("url", server.url.clone());
    out.set("description", server.description.clone());
    out.set_map("variables", server.variables.as_ref(), |variable| {
        server_variable_to_value(variable)
    });
    out.extend(&server.extensions);
    out.finish()
}

fn server_variable_to_value(variable: &ServerVariable) -> Value {
    let mut out = Builder::new();
    out.set_array("enum", variable.enumeration.as_deref(), |value| {
        Value::String(value.clone())
    });
    out.set("default", variable.default.clone());
    out.set("description", variable.description.clone());
    out.extend(&variable.extensions);
    out.finish()
}

fn tag_to_value(tag: &Tag) -> Value {
    let mut out = Builder::new();
    out.set("name", tag.name.clone());
    out.set("description", tag.description.clone());
    out.set_with(
        "externalDocs",
        tag.external_docs.as_ref(),
        schema_serialize::external_docs_to_value,
    );
    out.extend(&tag.extensions);
    out.finish()
}

fn security_requirement_to_value(requirement: &SecurityRequirement) -> Value {
    Value::Object(
        requirement
            .iter()
            .map(|(name, scopes)| {
                let scopes = scopes.iter().map(|s| Value::String(s.clone())).collect();
                (name.clone(), Value::Array(scopes))
            })
            .collect(),
    )
}

fn paths_to_value(store: &SchemaStore, paths: &Paths) -> Value {
    let mut out = Builder::new();
    for (template, item) in &paths.items {
        out.set(
            template,
            Some(maybe_ref_to_value(item, |item| {
                path_item_to_value(store, item)
            })),
        );
    }
    out.extend(&paths.extensions);
    out.finish()
}

fn path_item_to_value(store: &SchemaStore, item: &PathItem) -> Value {
    let mut out = Builder::new();
    out.set("summary", item.summary.clone());
    out.set("description", item.description.clone());
    for (method, operation) in item.operations() {
        out.set_with(method.slug(), Some(operation), |operation| {
            operation_to_value(store, operation)
        });
    }
    out.set_array("servers", item.servers.as_deref(), server_to_value);
    out.set_with("parameters", item.parameters.as_deref(), |parameters| {
        maybe_ref_array_to_value(parameters, |parameter| parameter_to_value(store, parameter))
    });
    out.extend(&item.extensions);
    out.finish()
}

fn operation_to_value(store: &SchemaStore, operation: &Operation) -> Value {
    let mut out = Builder::new();
    out.set_array("tags", operation.tags.as_deref(), |tag| {
        Value::String(tag.clone())
    });
    out.set("summary", operation.summary.clone());
    out.set("description", operation.description.clone());
    out.set_with(
        "externalDocs",
        operation.external_docs.as_ref(),
        schema_serialize::external_docs_to_value,
    );
    out.set("operationId", operation.id.clone());
    out.set_with(
        "parameters",
        operation.parameters.as_deref(),
        |parameters| {
            maybe_ref_array_to_value(parameters, |parameter| parameter_to_value(store, parameter))
        },
    );
    out.set_with("requestBody", operation.request_body.as_ref(), |body| {
        maybe_ref_to_value(body, |body| request_body_to_value(store, body))
    });
    out.set_with("responses", operation.responses.as_ref(), |responses| {
        responses_to_value(store, responses)
    });
    out.set_with("callbacks", operation.callbacks.as_ref(), |callbacks| {
        maybe_ref_map_to_value(callbacks, |callback| callback_to_value(store, callback))
    });
    out.set("deprecated", operation.deprecated);
    out.set_array(
        "security",
        operation.security.as_deref(),
        security_requirement_to_value,
    );
    out.set_array("servers", operation.servers.as_deref(), server_to_value);
    out.extend(&operation.extensions);
    out.finish()
}

fn parameter_to_value(store: &SchemaStore, parameter: &Parameter) -> Value {
    let mut out = Builder::new();
    out.set("name", parameter.name.clone());
    out.set("in", parameter.location.clone());
    out.set("description", parameter.description.clone());
    out.set("required", parameter.required);
    out.set("deprecated", parameter.deprecated);
    out.set("allowEmptyValue", parameter.allow_empty_value);
    out.set("style", parameter.style.clone());
    out.set("explode", parameter.explode);
    out.set("allowReserved", parameter.allow_reserved);
    out.set_with("schema", parameter.schema, |id| {
        schema_serialize::schema(store, id)
    });
    out.set("example", parameter.example.clone());
    out.set_with("examples", parameter.examples.as_ref(), |examples| {
        maybe_ref_map_to_value(examples, example_to_value)
    });
    out.set_with("content", parameter.content.as_ref(), |content| {
        content_to_value(store, content)
    });
    out.extend(&parameter.extensions);
    out.finish()
}

fn header_to_value(store: &SchemaStore, header: &Header) -> Value {
    let mut out = Builder::new();
    out.set("description", header.description.clone());
    out.set("required", header.required);
    out.set("deprecated", header.deprecated);
    out.set("allowEmptyValue", header.allow_empty_value);
    out.set("style", header.style.clone());
    out.set("explode", header.explode);
    out.set("allowReserved", header.allow_reserved);
    out.set_with("schema", header.schema, |id| {
        schema_serialize::schema(store, id)
    });
    out.set("example", header.example.clone());
    out.set_with("examples", header.examples.as_ref(), |examples| {
        maybe_ref_map_to_value(examples, example_to_value)
    });
    out.set_with("content", header.content.as_ref(), |content| {
        content_to_value(store, content)
    });
    out.extend(&header.extensions);
    out.finish()
}

fn request_body_to_value(store: &SchemaStore, body: &RequestBody) -> Value {
    let mut out = Builder::new();
    out.set("description", body.description.clone());
    out.set_with("content", body.content.as_ref(), |content| {
        content_to_value(store, content)
    });
    out.set("required", body.required);
    out.extend(&body.extensions);
    out.finish()
}

fn content_to_value(store: &SchemaStore, content: &BTreeMap<String, MediaType>) -> Value {
    Value::Object(
        content
            .iter()
            .map(|(name, media_type)| (name.clone(), media_type_to_value(store, media_type)))
            .collect(),
    )
}

fn media_type_to_value(store: &SchemaStore, media_type: &MediaType) -> Value {
    let mut out = Builder::new();
    out.set_with("schema", media_type.schema, |id| {
        schema_serialize::schema(store, id)
    });
    out.set("example", media_type.example.clone());
    out.set_with("examples", media_type.examples.as_ref(), |examples| {
        maybe_ref_map_to_value(examples, example_to_value)
    });
    out.set_map("encoding", media_type.encoding.as_ref(), |encoding| {
        encoding_to_value(store, encoding)
    });
    out.extend(&media_type.extensions);
    out.finish()
}

fn encoding_to_value(store: &SchemaStore, encoding: &Encoding) -> Value {
    let mut out = Builder::new();
    out.set("contentType", encoding.content_type.clone());
    out.set_with("headers", encoding.headers.as_ref(), |headers| {
        maybe_ref_map_to_value(headers, |header| header_to_value(store, header))
    });
    out.set("style", encoding.style.clone());
    out.set("explode", encoding.explode);
    out.set("allowReserved", encoding.allow_reserved);
    out.extend(&encoding.extensions);
    out.finish()
}

fn responses_to_value(store: &SchemaStore, responses: &Responses) -> Value {
    let mut out = Builder::new();
    out.set_with("default", responses.default.as_ref(), |response| {
        maybe_ref_to_value(response, |response| response_to_value(store, response))
    });
    for (status, response) in &responses.statuses {
        out.set(
            status,
            Some(maybe_ref_to_value(response, |response| {
                response_to_value(store, response)
            })),
        );
    }
    out.extend(&responses.extensions);
    out.finish()
}

fn response_to_value(store: &SchemaStore, response: &Response) -> Value {
    let mut out = Builder::new();
    out.set("description", response.description.clone());
    out.set_with("headers", response.headers.as_ref(), |headers| {
        maybe_ref_map_to_value(headers, |header| header_to_value(store, header))
    });
    out.set_with("content", response.content.as_ref(), |content| {
        content_to_value(store, content)
    });
    out.set_with("links", response.links.as_ref(), |links| {
        maybe_ref_map_to_value(links, link_to_value)
    });
    out.extend(&response.extensions);
    out.finish()
}

fn callback_to_value(store: &SchemaStore, callback: &Callback) -> Value {
    let mut out = Builder::new();
    for (expression, item) in &callback.expressions {
        out.set(
            expression,
            Some(maybe_ref_to_value(item, |item| {
                path_item_to_value(store, item)
            })),
        );
    }
    out.extend(&callback.extensions);
    out.finish()
}

fn example_to_value(example: &Example) -> Value {
    let mut out = Builder::new();
    out.set("summary", example.summary.clone());
    out.set("description", example.description.clone());
    out.set("value", example.value.clone());
    out.set("externalValue", example.external_value.clone());
    out.extend(&example.extensions);
    out.finish()
}

fn link_to_value(link: &Link) -> Value {
    let mut out = Builder::new();
    out.set("operationRef", link.operation_ref.clone());
    out.set("operationId", link.operation_id.clone());
    out.set_map("parameters", link.parameters.as_ref(), Clone::clone);
    out.set("requestBody", link.request_body.clone());
    out.set("description", link.description.clone());
    out.set_with("server", link.server.as_ref(), server_to_value);
    out.extend(&link.extensions);
    out.finish()
}

fn security_scheme_to_value(scheme: &SecurityScheme) -> Value {
    let mut out = Builder::new();
    out.set("type", scheme.kind.clone());
    out.set("description", scheme.description.clone());
    out.set("name", scheme.name.clone());
    out.set("in", scheme.location.clone());
    out.set("scheme", scheme.scheme.clone());
    out.set("bearerFormat", scheme.bearer_format.clone());
    out.set_with("flows", scheme.flows.as_ref(), oauth_flows_to_value);
    out.set("openIdConnectUrl", scheme.open_id_connect_url.clone());
    out.extend(&scheme.extensions);
    out.finish()
}

fn oauth_flows_to_value(flows: &OAuthFlows) -> Value {
    let mut out = Builder::new();
    out.set_with("implicit", flows.implicit.as_ref(), oauth_flow_to_value);
    out.set_with("password", flows.password.as_ref(), oauth_flow_to_value);
    out.set_with(
        "clientCredentials",
        flows.client_credentials.as_ref(),
        oauth_flow_to_value,
    );
    out.set_with(
        "authorizationCode",
        flows.authorization_code.as_ref(),
        oauth_flow_to_value,
    );
    out.extend(&flows.extensions);
    out.finish()
}

fn oauth_flow_to_value(flow: &OAuthFlow) -> Value {
    let mut out = Builder::new();
    out.set("authorizationUrl", flow.authorization_url.clone());
    out.set("tokenUrl", flow.token_url.clone());
    out.set("refreshUrl", flow.refresh_url.clone());
    out.set_map("scopes", flow.scopes.as_ref(), |description| {
        Value::String(description.clone())
    });
    out.extend(&flow.extensions);
    out.finish()
}

fn components_to_value(store: &SchemaStore, components: &Components) -> Value {
    let mut out = Builder::new();
    out.set_with("schemas", components.schemas.as_ref(), |schemas| {
        schema_serialize::schema_map(store, schemas)
    });
    out.set_with("responses", components.responses.as_ref(), |responses| {
        maybe_ref_map_to_value(responses, |response| response_to_value(store, response))
    });
    out.set_with("parameters", components.parameters.as_ref(), |parameters| {
        maybe_ref_map_to_value(parameters, |parameter| parameter_to_value(store, parameter))
    });
    out.set_with("examples", components.examples.as_ref(), |examples| {
        maybe_ref_map_to_value(examples, example_to_value)
    });
    out.set_with(
        "requestBodies",
        components.request_bodies.as_ref(),
        |bodies| maybe_ref_map_to_value(bodies, |body| request_body_to_value(store, body)),
    );
    out.set_with("headers", components.headers.as_ref(), |headers| {
        maybe_ref_map_to_value(headers, |header| header_to_value(store, header))
    });
    out.set_with(
        "securitySchemes",
        components.security_schemes.as_ref(),
        |schemes| maybe_ref_map_to_value(schemes, security_scheme_to_value),
    );
    out.set_with("links", components.links.as_ref(), |links| {
        maybe_ref_map_to_value(links, link_to_value)
    });
    out.set_with("callbacks", components.callbacks.as_ref(), |callbacks| {
        maybe_ref_map_to_value(callbacks, |callback| callback_to_value(store, callback))
    });
    out.set_with("pathItems", components.path_items.as_ref(), |items| {
        maybe_ref_map_to_value(items, |item| path_item_to_value(store, item))
    });
    out.extend(&components.extensions);
    out.finish()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::diag::Ctx;
    use crate::normalize;

    /// A document exercising **every member the parser models**, on every node type.
    ///
    /// The schema layer pins its parse↔serialize mirror with `EVERY_KEYWORD`; this is the same
    /// pin for the document layer, which had none — a member read by `parse.rs` and forgotten by
    /// `serialize.rs` was invisible to every gate except corpus luck, and losslessness is this
    /// crate's central claim. When you teach the parser a member, add it here: a member missing
    /// from this fixture is a member whose serializer no test holds.
    #[expect(
        clippy::too_many_lines,
        reason = "one maximal document, spelled member by member; splitting it would only hide members"
    )]
    fn maximal() -> Value {
        json!({
            "openapi": "3.1.0",
            "jsonSchemaDialect": "https://spec.openapis.org/oas/3.1/dialect/base",
            "info": {
                "title": "Everything", "summary": "All of it", "description": "Every member.",
                "termsOfService": "https://example.invalid/terms", "version": "1.2.3",
                "contact": {"name": "A", "url": "https://example.invalid", "email": "a@example.invalid", "x-contact": 1},
                "license": {"name": "MIT", "identifier": "MIT", "url": "https://example.invalid/mit", "x-license": 2},
                "x-info": 3,
            },
            "servers": [{
                "url": "https://{region}.example.invalid", "description": "one",
                "variables": {"region": {"enum": ["eu", "us"], "default": "eu", "description": "where", "x-variable": 4}},
                "x-server": 5,
            }],
            "paths": {
                "/pets/{id}": {
                    "summary": "One route", "description": "Its story.",
                    "get": {
                        "tags": ["pets"], "summary": "Read", "description": "Reads.",
                        "externalDocs": {"description": "More", "url": "https://example.invalid/docs"},
                        "operationId": "getPet",
                        "parameters": [
                            {"name": "id", "in": "path", "description": "which", "required": true,
                             "deprecated": false, "allowEmptyValue": false, "style": "simple",
                             "explode": false, "allowReserved": false,
                             "schema": {"type": "string"}, "example": "seven", "x-parameter": 6},
                            {"name": "filter", "in": "query",
                             "examples": {"one": {"summary": "an example", "description": "of it",
                                                  "value": {"a": 1}, "externalValue": "https://example.invalid/ex",
                                                  "x-example": 7}},
                             "content": {"application/json": {"schema": {"type": "object"}}}},
                        ],
                        "requestBody": {
                            "description": "the body", "required": true,
                            "content": {"application/x-www-form-urlencoded": {
                                "schema": {"type": "object"},
                                "example": {"a": 1},
                                "examples": {"one": {"value": {"a": 2}}},
                                "encoding": {"a": {
                                    "contentType": "text/plain",
                                    "headers": {"X-Rate": {"description": "a header", "required": true,
                                                            "deprecated": false, "allowEmptyValue": false,
                                                            "style": "simple", "explode": false,
                                                            "allowReserved": false,
                                                            "schema": {"type": "integer"},
                                                            "example": 3, "x-header": 8}},
                                    "style": "form", "explode": true, "allowReserved": false,
                                    "x-encoding": 9,
                                }},
                                "x-media": 10,
                            }},
                            "x-body": 11,
                        },
                        "responses": {
                            "default": {"description": "fallback"},
                            "200": {
                                "description": "ok",
                                "headers": {"X-Total": {"schema": {"type": "integer"}}},
                                "content": {"application/json": {"schema": {"type": "string"}}},
                                "links": {"next": {"operationRef": "#/paths/~1pets~1{id}/get",
                                                    "operationId": "getPet",
                                                    "parameters": {"id": "$response.body#/next"},
                                                    "requestBody": "$response.body#/body",
                                                    "description": "the next one",
                                                    "server": {"url": "https://example.invalid"},
                                                    "x-link": 12}},
                                "x-response": 13,
                            },
                            "x-responses": 14,
                        },
                        "callbacks": {"onEvent": {"{$request.body#/url}": {"post": {
                            "responses": {"200": {"description": "ok"}},
                        }}, "x-callback": 15}},
                        "deprecated": true,
                        "security": [{"petstore_auth": ["read:pets"]}],
                        "servers": [{"url": "https://op.example.invalid"}],
                        "x-operation": 16,
                    },
                    "put": {"responses": {"200": {"description": "ok"}}},
                    "post": {"responses": {"200": {"description": "ok"}}},
                    "delete": {"responses": {"200": {"description": "ok"}}},
                    "options": {"responses": {"200": {"description": "ok"}}},
                    "head": {"responses": {"200": {"description": "ok"}}},
                    "patch": {"responses": {"200": {"description": "ok"}}},
                    "trace": {"responses": {"200": {"description": "ok"}}},
                    "servers": [{"url": "https://item.example.invalid"}],
                    "parameters": [{"name": "trace", "in": "header", "schema": {"type": "string"}}],
                    "x-item": 17,
                },
                "x-paths": 18,
            },
            "webhooks": {
                "newPet": {"$ref": "#/components/pathItems/Hook", "summary": "a hooked item",
                            "description": "reference members survive", "x-ref": 19},
            },
            "components": {
                "schemas": {"Pet": {"type": "object", "properties": {"name": {"type": "string"}}}},
                "responses": {"Err": {"description": "an error"}},
                "parameters": {"Page": {"name": "page", "in": "query", "schema": {"type": "integer"}}},
                "examples": {"One": {"value": 1}},
                "requestBodies": {"Body": {"content": {"application/json": {"schema": {"type": "object"}}}}},
                "headers": {"Rate": {"schema": {"type": "integer"}}},
                "securitySchemes": {"petstore_auth": {
                    "type": "oauth2", "description": "the flows", "name": "auth", "in": "header",
                    "scheme": "bearer", "bearerFormat": "JWT",
                    "flows": {
                        "implicit": {"authorizationUrl": "https://example.invalid/auth",
                                      "tokenUrl": "https://example.invalid/token",
                                      "refreshUrl": "https://example.invalid/refresh",
                                      "scopes": {"read:pets": "read"}, "x-flow": 20},
                        "password": {"tokenUrl": "https://example.invalid/token", "scopes": {}},
                        "clientCredentials": {"tokenUrl": "https://example.invalid/token", "scopes": {}},
                        "authorizationCode": {"authorizationUrl": "https://example.invalid/auth",
                                               "tokenUrl": "https://example.invalid/token", "scopes": {}},
                        "x-flows": 21,
                    },
                    "openIdConnectUrl": "https://example.invalid/oidc",
                    "x-scheme": 22,
                }},
                "links": {"Next": {"operationId": "getPet"}},
                "callbacks": {"Cb": {"{$url}": {"get": {"responses": {"200": {"description": "ok"}}}}}},
                "pathItems": {"Hook": {"post": {"responses": {"200": {"description": "ok"}}}}},
                "x-components": 23,
            },
            "security": [{"petstore_auth": ["write:pets", "read:pets"]}],
            "tags": [{"name": "pets", "description": "the pets",
                       "externalDocs": {"description": "more", "url": "https://example.invalid/tags"},
                       "x-tag": 24}],
            "externalDocs": {"description": "everything else", "url": "https://example.invalid/all"},
            "x-root": 25,
        })
    }

    /// Parse and serialize are member-by-member mirrors, and this is the test that holds them to
    /// it. Equality with the input proves no modelled member is dropped on the way out; the
    /// members the parser does not model round-trip through `extensions` and prove nothing here.
    #[test]
    fn a_maximal_document_round_trips_member_by_member() {
        let mut ctx = Ctx::new();
        let input = maximal();
        let normalized = normalize::normalize(input.clone(), &mut ctx).unwrap();
        let parsed = super::super::parse::document(normalized, &mut ctx);
        let output = super::document(&parsed);
        assert_eq!(output, input);
    }

    /// The normalizer's method list is the document model's, spelled as wire members.
    ///
    /// `normalize` sits below `doc` and walks raw values, so it cannot use
    /// [`super::super::PathItem::operations`]; this pins the two lists to each other instead.
    #[test]
    fn the_normalizer_walks_every_method_the_model_holds() {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(maximal(), &mut ctx).unwrap();
        let parsed = super::super::parse::document(normalized, &mut ctx);
        let paths = parsed.document.paths.as_ref().unwrap();
        let super::super::MaybeRef::Item(item) = &paths.items["/pets/{id}"] else {
            panic!("the fixture writes the path item inline");
        };
        let walked: Vec<&str> = item.operations().map(|(method, _)| method.slug()).collect();
        assert_eq!(walked, normalize::METHODS);
    }
}
