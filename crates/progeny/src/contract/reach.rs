//! Which direction each type travels: out with a request, back in a response, both, or neither.
//!
//! Decided from the document's own positions — request bodies, parameters, the JSON response
//! arms and the response headers of every operation, or a JSON Schema bundle's roots — and closed
//! over the type graph, because a body names one type and that type names others.
//! The answer is what tells a type apart by the rules it is decoded under: a request is
//! data the caller built, a response is whatever the vendor sent.
//!
//! The seeding is deliberately a little broad.
//! Every JSON media type a response position declares counts, not only the one the API
//! model will pick, because the two layers cannot be asked in that order and erring broad
//! costs at most a read form nothing decodes into; erring narrow would decode a response
//! into a type with no read form at all.
//! Every declared response header counts for the same reason, whether or not the API model
//! gives it a typed field; only the `Content-Type` declaration OpenAPI says to ignore does not.

use std::collections::{BTreeMap, BTreeSet};

use super::{TypeIndex, TypeRef};
use crate::doc::{
    Header, MaybeRef, MediaType, Parameter, ignored_response_header, is_json_media_type,
};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::ShapeKey;

/// Which directions a type is reachable in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Reach {
    pub(crate) request: bool,
    pub(crate) response: bool,
}

/// The reach of every type, by index.
pub(super) fn run(
    resolved: &ResolvedDocument,
    by_shape: &BTreeMap<ShapeKey, TypeRef>,
    references: impl Fn(TypeIndex) -> Vec<TypeIndex>,
    count: usize,
) -> Vec<Reach> {
    let mut requests = BTreeSet::new();
    let mut responses = BTreeSet::new();
    let seed = |key: ShapeKey, out: &mut BTreeSet<TypeIndex>| {
        if let Some(ty) = by_shape.get(&key) {
            ty.named_into(out);
        }
    };
    let parameter_keys = |node: &MaybeRef<Parameter>| {
        resolved.parameter(node).map_or_else(Vec::new, |parameter| {
            value_keys(resolved, parameter.schema, parameter.content.as_ref())
        })
    };
    // A header's value is read the way a parameter's is.
    let header_keys = |node: &MaybeRef<Header>| {
        resolved.header(node).map_or_else(Vec::new, |header| {
            value_keys(resolved, header.schema, header.content.as_ref())
        })
    };

    // Paths only, like the API model: a webhook is an operation nobody here can call, and its
    // bodies travel the other way.
    let items = resolved
        .document()
        .paths
        .as_ref()
        .map(|paths| &paths.items)
        .into_iter()
        .flatten();
    for (_, item) in items {
        let Some(item) = resolved.path_item(item) else {
            continue;
        };
        for node in item.parameters.iter().flatten() {
            for key in parameter_keys(node) {
                seed(key, &mut requests);
            }
        }
        for (_, operation) in item.operations() {
            for node in operation.parameters.iter().flatten() {
                for key in parameter_keys(node) {
                    seed(key, &mut requests);
                }
            }
            // Every media type of a request body is a request position: a multipart or form
            // body carries its type out exactly as a JSON one does.
            if let Some(node) = &operation.request_body
                && let Some(body) = resolved.request_body(node)
            {
                for key in content_keys(resolved, body.content.as_ref(), false) {
                    seed(key, &mut requests);
                }
            }
            let arms = operation.responses.iter().flat_map(|responses| {
                responses
                    .statuses
                    .values()
                    .chain(responses.default.as_ref())
            });
            for node in arms {
                let Some(response) = resolved.response(node) else {
                    continue;
                };
                for key in content_keys(resolved, response.content.as_ref(), true) {
                    seed(key, &mut responses);
                }
                let headers = response.headers.iter().flatten();
                for (_, node) in headers.filter(|(name, _)| !ignored_response_header(name)) {
                    for key in header_keys(node) {
                        seed(key, &mut responses);
                    }
                }
            }
        }
    }

    // A bundle's roots, each in the direction the caller exchanges it, in place of operations.
    for root in resolved.bundle_roots() {
        let key = crate::shape::key_of(resolved, root.id);
        if root.direction.sent() {
            seed(key.clone(), &mut requests);
        }
        if root.direction.received() {
            seed(key, &mut responses);
        }
    }

    let requests = close(requests, &references);
    let responses = close(responses, &references);
    (0..count)
        .map(|index| {
            let index = TypeIndex(u32::try_from(index).unwrap_or(u32::MAX));
            Reach {
                request: requests.contains(&index),
                response: responses.contains(&index),
            }
        })
        .collect()
}

/// The schemas of a media-type map, or only of its JSON media types.
fn content_keys(
    resolved: &ResolvedDocument,
    content: Option<&BTreeMap<String, MediaType>>,
    json_only: bool,
) -> Vec<ShapeKey> {
    content
        .into_iter()
        .flatten()
        .filter(|(media_type, _)| !json_only || is_json_media_type(media_type))
        .filter_map(|(_, entry)| entry.schema)
        .map(|id| crate::shape::key_of(resolved, id))
        .collect()
}

/// The schemas a parameter or header value is read through: its `schema`, or those of the media
/// types its `content` declares.
fn value_keys(
    resolved: &ResolvedDocument,
    schema: Option<SchemaId>,
    content: Option<&BTreeMap<String, MediaType>>,
) -> Vec<ShapeKey> {
    let mut keys: Vec<ShapeKey> = schema
        .map(|id| crate::shape::key_of(resolved, id))
        .into_iter()
        .collect();
    keys.extend(content_keys(resolved, content, false));
    keys
}

/// Everything reachable from a starting set, following the types those types name.
fn close(
    start: BTreeSet<TypeIndex>,
    references: &impl Fn(TypeIndex) -> Vec<TypeIndex>,
) -> BTreeSet<TypeIndex> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<TypeIndex> = start.into_iter().collect();
    while let Some(index) = queue.pop() {
        if !seen.insert(index) {
            continue;
        }
        queue.extend(references(index));
    }
    seen
}

impl TypeRef {
    /// Every named type this reference reaches, added to a set.
    fn named_into(&self, out: &mut BTreeSet<TypeIndex>) {
        let mut reached = Vec::new();
        self.named(&mut reached);
        out.extend(reached);
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::json;

    use super::super::Form;
    use super::super::tests::{contracts_of, named};
    use crate::config::Config;

    /// A schema only a response header names is a response position, so it takes the read form
    /// a client will decode the header into, through a `schema` or a JSON `content` alike.
    /// The `Content-Type` declaration OpenAPI says to ignore reaches nothing.
    #[test_util::test]
    fn a_schema_only_a_response_header_names_is_read() -> eyre::Result<()> {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {"/things": {"get": {
                "operationId": "getThings",
                "responses": {"204": {"description": "none", "headers": {
                    "X-Mode": {"schema": {"$ref": "#/components/schemas/Mode"}},
                    "X-Shape": {"content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Shape"},
                    }}},
                    "Content-Type": {"schema": {"$ref": "#/components/schemas/Media"}},
                }}},
            }}},
            "components": {"schemas": {
                "Mode": {"type": "string", "enum": ["fast", "slow"]},
                "Shape": {"type": "object", "required": ["side"],
                          "properties": {"side": {"type": "integer"}}},
                "Media": {"type": "string", "enum": ["application/json"]},
            }},
        });
        let (contracts, _) = contracts_of(document, &Config::default())?;
        let response_only = Form::Lenient { strict: None };
        assert_eq!(named(&contracts, "Mode")?.form(), response_only);
        assert_eq!(named(&contracts, "Shape")?.form(), response_only);
        assert_eq!(
            named(&contracts, "Media")?.form(),
            Form::Strict { twin: None }
        );
        assert!(contracts.has_read_forms());
        Ok(())
    }
}
