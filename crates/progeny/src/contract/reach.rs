//! Which direction each type travels: out with a request, back in a response, both, or neither.
//!
//! Decided from the document's own positions — request bodies, parameters, and the JSON
//! response arms of every operation — and closed over the type graph, because a body
//! names one type and that type names others.
//! The answer is what tells a type apart by the rules it is decoded under: a request is
//! data the caller built, a response is whatever the vendor sent.
//!
//! The seeding is deliberately a little broad.
//! Every JSON media type a response position declares counts, not only the one the API
//! model will pick, because the two layers cannot be asked in that order and erring broad
//! costs at most a read form nothing decodes into; erring narrow would decode a response
//! into a type with no read form at all.

use std::collections::{BTreeMap, BTreeSet};

use super::{TypeIndex, TypeRef};
use crate::doc::{MaybeRef, MediaType, Parameter, is_json_media_type};
use crate::resolve::ResolvedDocument;
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
    let content_keys = |content: Option<&BTreeMap<String, MediaType>>, json_only: bool| {
        content
            .into_iter()
            .flatten()
            .filter(move |(media_type, _)| !json_only || is_json_media_type(media_type))
            .filter_map(|(_, entry)| entry.schema)
            .map(|id| crate::shape::key_of(resolved, id))
            .collect::<Vec<_>>()
    };
    let parameter_keys = |node: &MaybeRef<Parameter>| {
        let Some(parameter) = resolved.parameter(node) else {
            return Vec::new();
        };
        let mut keys: Vec<ShapeKey> = parameter
            .schema
            .map(|id| crate::shape::key_of(resolved, id))
            .into_iter()
            .collect();
        keys.extend(content_keys(parameter.content.as_ref(), false));
        keys
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
                for key in content_keys(body.content.as_ref(), false) {
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
                for key in content_keys(response.content.as_ref(), true) {
                    seed(key, &mut responses);
                }
            }
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
