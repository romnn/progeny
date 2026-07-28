//! Counting what a parsed document contains.
//!
//! Once a document is a value rather than text, the questions that decide later design choices
//! become one traversal each: how many `anyOf`s are really nullable emulation, how many integers
//! carry bounds worth narrowing a width for, whether any reference addresses another file. This
//! is the quieter half of the argument for building the model first — it turns the corpus into a
//! queryable dataset.

use std::collections::{BTreeMap, BTreeSet};

use crate::diag::{Ctx, RejectError};
use crate::doc::{
    Callback, Components, MaybeRef, Operation, ParsedDocument, PathItem, Response, Responses,
};
use crate::schema::{OneOrMany, Schema, SchemaId, SchemaObject, SchemaStore, TypeName};
use crate::{doc, load, normalize};

/// Counts over one parsed document, for the questions the corpus is the evidence base for.
///
/// Each field answers a question that decides a later design choice, and each is cheap to compute
/// once the document is a value rather than text. Adding a count here is how a design argument
/// stops being a matter of opinion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Schemas in the document.
    pub schemas: usize,
    /// `anyOf` occurrences, and the pattern each one is.
    pub any_of: AnyOfShapes,
    /// `oneOf` occurrences.
    pub one_of: usize,
    /// `oneOf`/`anyOf` occurrences carrying a discriminator.
    pub discriminated: usize,
    /// Properties that are both optional and nullable, where absent and `null` are different
    /// documents and a two-state `Option` cannot say which.
    pub optional_and_nullable: usize,
    /// Integer schemas carrying a bound, which is what would justify picking a width from bounds
    /// rather than a flat `i64`/`u64`.
    pub bounded_integers: usize,
    /// Integer schemas in total.
    pub integers: usize,
    /// Operations whose request body declares more than one media type.
    pub multi_content_operations: usize,
    /// Responses declaring headers.
    pub responses_with_headers: usize,
    /// Response headers in total.
    pub response_headers: usize,
    /// Security scheme kinds, by their `type`.
    pub security_scheme_kinds: BTreeMap<String, usize>,
    /// `$ref` strings that address another file rather than this document.
    pub external_refs: usize,
    /// `$dynamicRef` and `$dynamicAnchor` occurrences.
    pub dynamic_scoping: usize,
    /// Non-root `$id` occurrences, which change the base URI a relative reference resolves
    /// against.
    pub nested_ids: usize,
    /// `patternProperties` occurrences.
    pub pattern_properties: usize,
    /// `prefixItems` occurrences.
    pub prefix_items: usize,
    /// `const` occurrences.
    pub constants: usize,
    /// The deepest schema nesting reached.
    pub max_schema_depth: usize,
}

/// How each `anyOf` in a document is shaped.
///
/// The union policy turns on this histogram: "any combination may match" has no faithful Rust
/// type, but the overwhelming majority of real `anyOf`s are not asking for that — they are
/// emulating a nullable type or an enumeration, and those have exact translations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnyOfShapes {
    /// Occurrences in total.
    pub total: usize,
    /// `[T, {"type": "null"}]` — a nullable `T`.
    pub nullable: usize,
    /// Every branch is a `const` or a single-valued `enum` — an enumeration.
    pub constants: usize,
    /// Every branch declares a different `type` — distinguishable by shape.
    pub disjoint_types: usize,
    /// Everything else, which is where degradation lives.
    pub other: usize,
}

impl Stats {
    /// Fold another document's counts into these, so the corpus can be read as one dataset.
    ///
    /// Maxima are taken rather than summed; everything else adds.
    pub fn merge(&mut self, other: &Self) {
        self.schemas += other.schemas;
        self.any_of.total += other.any_of.total;
        self.any_of.nullable += other.any_of.nullable;
        self.any_of.constants += other.any_of.constants;
        self.any_of.disjoint_types += other.any_of.disjoint_types;
        self.any_of.other += other.any_of.other;
        self.one_of += other.one_of;
        self.discriminated += other.discriminated;
        self.optional_and_nullable += other.optional_and_nullable;
        self.bounded_integers += other.bounded_integers;
        self.integers += other.integers;
        self.multi_content_operations += other.multi_content_operations;
        self.responses_with_headers += other.responses_with_headers;
        self.response_headers += other.response_headers;
        for (kind, count) in &other.security_scheme_kinds {
            *self.security_scheme_kinds.entry(kind.clone()).or_default() += count;
        }
        self.external_refs += other.external_refs;
        self.dynamic_scoping += other.dynamic_scoping;
        self.nested_ids += other.nested_ids;
        self.pattern_properties += other.pattern_properties;
        self.prefix_items += other.prefix_items;
        self.constants += other.constants;
        self.max_schema_depth = self.max_schema_depth.max(other.max_schema_depth);
    }
}

/// Count what one document contains.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn stats(input: &[u8]) -> Result<Stats, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    Ok(collect(&parsed))
}

fn collect(parsed: &ParsedDocument) -> Stats {
    let mut stats = Stats {
        schemas: parsed.schemas.len(),
        ..Stats::default()
    };
    for (_, schema) in parsed.schemas.iter() {
        if let Schema::Object(object) = schema {
            schema_object(object, &parsed.schemas, &mut stats);
        }
    }
    document(parsed, &mut stats);
    stats.max_schema_depth = max_depth(parsed);
    stats
}

fn schema_object(object: &SchemaObject, store: &SchemaStore, stats: &mut Stats) {
    if let Some(branches) = &object.any_of {
        stats.any_of.total += 1;
        classify_any_of(branches, store, &mut stats.any_of);
    }
    if object.one_of.is_some() {
        stats.one_of += 1;
    }
    if object.discriminator.is_some() {
        stats.discriminated += 1;
    }
    if object.pattern_properties.is_some() {
        stats.pattern_properties += 1;
    }
    if object.prefix_items.is_some() {
        stats.prefix_items += 1;
    }
    if object.constant.is_some() {
        stats.constants += 1;
    }
    if object.dynamic_reference.is_some() {
        stats.dynamic_scoping += 1;
    }
    if object.dynamic_anchor.is_some() {
        stats.dynamic_scoping += 1;
    }
    if object.id.is_some() {
        stats.nested_ids += 1;
    }
    if let Some(reference) = &object.reference
        && is_external(reference)
    {
        stats.external_refs += 1;
    }
    if declares(object, &TypeName::Integer) {
        stats.integers += 1;
        let bounded = object.maximum.is_some()
            || object.minimum.is_some()
            || object.exclusive_maximum.is_some()
            || object.exclusive_minimum.is_some();
        if bounded {
            stats.bounded_integers += 1;
        }
    }
    count_optional_nullable(object, store, stats);
}

/// A property that is both absent-able and null-able: three states, and a bare `Option` has two.
fn count_optional_nullable(object: &SchemaObject, store: &SchemaStore, stats: &mut Stats) {
    let Some(properties) = &object.properties else {
        return;
    };
    let required: BTreeSet<&str> = object
        .required
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    for (name, id) in properties {
        if required.contains(name.as_str()) {
            continue;
        }
        if let Schema::Object(property) = store.get(*id)
            && declares(property, &TypeName::Null)
        {
            stats.optional_and_nullable += 1;
        }
    }
}

fn declares(object: &SchemaObject, wanted: &TypeName) -> bool {
    object
        .types
        .as_ref()
        .is_some_and(|types| types.iter().any(|name| name == wanted))
}

fn classify_any_of(branches: &[SchemaId], store: &SchemaStore, shapes: &mut AnyOfShapes) {
    let objects: Vec<Option<&SchemaObject>> = branches
        .iter()
        .map(|id| match store.get(*id) {
            Schema::Object(object) => Some(object.as_ref()),
            Schema::Bool(_) => None,
        })
        .collect();

    let null_branches = objects
        .iter()
        .filter(|object| object.is_some_and(|object| only_type(object, &TypeName::Null)))
        .count();
    if branches.len() == 2 && null_branches == 1 {
        shapes.nullable += 1;
        return;
    }

    let all_constant = !objects.is_empty()
        && objects.iter().all(|object| {
            object.is_some_and(|object| {
                object.constant.is_some()
                    || object
                        .enumeration
                        .as_ref()
                        .is_some_and(|values| values.len() == 1)
            })
        });
    if all_constant {
        shapes.constants += 1;
        return;
    }

    let mut seen = BTreeSet::new();
    let disjoint = !objects.is_empty()
        && objects.iter().all(|object| {
            object.is_some_and(|object| match &object.types {
                Some(OneOrMany::One(name)) => seen.insert(name.clone()),
                _ => false,
            })
        });
    if disjoint {
        shapes.disjoint_types += 1;
        return;
    }

    shapes.other += 1;
}

fn only_type(object: &SchemaObject, wanted: &TypeName) -> bool {
    match &object.types {
        Some(types) => types.iter().all(|name| name == wanted),
        None => false,
    }
}

/// A reference that does not begin with `#` addresses another document.
fn is_external(reference: &str) -> bool {
    !reference.starts_with('#')
}

fn document(parsed: &ParsedDocument, stats: &mut Stats) {
    if let Some(paths) = &parsed.document.paths {
        for item in paths.items.values() {
            if let MaybeRef::Item(item) = item {
                path_item(item, stats);
            }
        }
    }
    for item in parsed.document.webhooks.iter().flatten().map(|(_, v)| v) {
        if let MaybeRef::Item(item) = item {
            path_item(item, stats);
        }
    }
    if let Some(components) = &parsed.document.components {
        components_node(components, stats);
    }
}

fn components_node(components: &Components, stats: &mut Stats) {
    for scheme in components.security_schemes.iter().flatten().map(|(_, v)| v) {
        if let MaybeRef::Item(scheme) = scheme {
            let kind = scheme.kind.clone().unwrap_or_else(|| "<absent>".to_owned());
            *stats.security_scheme_kinds.entry(kind).or_default() += 1;
        }
    }
    for response in components.responses.iter().flatten().map(|(_, v)| v) {
        if let MaybeRef::Item(response) = response {
            response_node(response, stats);
        }
    }
    for item in components.path_items.iter().flatten().map(|(_, v)| v) {
        if let MaybeRef::Item(item) = item {
            path_item(item, stats);
        }
    }
}

fn path_item(item: &PathItem, stats: &mut Stats) {
    for (_, operation) in item.operations() {
        operation_node(operation, stats);
    }
}

fn operation_node(operation: &Operation, stats: &mut Stats) {
    if let Some(MaybeRef::Item(body)) = &operation.request_body
        && let Some(content) = &body.content
        && content.len() > 1
    {
        stats.multi_content_operations += 1;
    }
    if let Some(responses) = &operation.responses {
        responses_node(responses, stats);
    }
    for callback in operation.callbacks.iter().flatten().map(|(_, v)| v) {
        if let MaybeRef::Item(callback) = callback {
            callback_node(callback, stats);
        }
    }
}

fn callback_node(callback: &Callback, stats: &mut Stats) {
    for item in callback.expressions.values() {
        if let MaybeRef::Item(item) = item {
            path_item(item, stats);
        }
    }
}

fn responses_node(responses: &Responses, stats: &mut Stats) {
    for response in responses
        .statuses
        .values()
        .chain(responses.default.as_ref())
    {
        if let MaybeRef::Item(response) = response {
            response_node(response, stats);
        }
    }
}

fn response_node(response: &Response, stats: &mut Stats) {
    if let Some(headers) = &response.headers
        && !headers.is_empty()
    {
        stats.responses_with_headers += 1;
        stats.response_headers += headers.len();
    }
}

/// The deepest schema nesting the document reaches, measured over the reachable graph.
///
/// A cycle would make "depth" unbounded, so each schema is entered at most once per path; the
/// visited set is what makes the traversal terminate on a recursive schema.
fn max_depth(parsed: &ParsedDocument) -> usize {
    let mut best = 0;
    let mut visiting = BTreeSet::new();
    for (id, _) in parsed.schemas.iter() {
        best = best.max(depth_from(id, &parsed.schemas, &mut visiting));
    }
    best
}

fn depth_from(id: SchemaId, store: &SchemaStore, visiting: &mut BTreeSet<SchemaId>) -> usize {
    if !visiting.insert(id) {
        return 0;
    }
    let deepest = match store.get(id) {
        Schema::Bool(_) => 0,
        Schema::Object(object) => children(object)
            .into_iter()
            .map(|child| depth_from(child, store, visiting))
            .max()
            .unwrap_or(0),
    };
    visiting.remove(&id);
    1 + deepest
}

fn children(object: &SchemaObject) -> Vec<SchemaId> {
    let singles = [
        object.not,
        object.if_schema,
        object.then_schema,
        object.else_schema,
        object.additional_properties,
        object.property_names,
        object.items,
        object.contains,
        object.unevaluated_items,
        object.unevaluated_properties,
        object.content_schema,
    ];
    let arrays = [
        object.all_of.as_deref(),
        object.any_of.as_deref(),
        object.one_of.as_deref(),
        object.prefix_items.as_deref(),
    ];
    let maps = [
        object.defs.as_ref(),
        object.dependent_schemas.as_ref(),
        object.properties.as_ref(),
        object.pattern_properties.as_ref(),
    ];

    let mut out: Vec<SchemaId> = singles.into_iter().flatten().collect();
    out.extend(arrays.into_iter().flatten().flatten().copied());
    out.extend(maps.into_iter().flatten().flatten().map(|(_, id)| *id));
    out
}

#[cfg(test)]
mod tests {
    use super::stats;

    #[test]
    fn the_committed_spec_can_be_counted() {
        const PETSTORE: &[u8] = include_bytes!("../../../../corpus/specs/petstore-31.yaml");
        let counted = stats(PETSTORE).unwrap();
        assert!(counted.schemas > 0);
        assert_eq!(counted.external_refs, 0);
        assert_eq!(counted.dynamic_scoping, 0);
    }

    #[test]
    fn a_nullable_any_of_is_recognized_as_nullable_emulation() {
        let document = br#"{
          "openapi": "3.1.0", "paths": {},
          "components": {"schemas": {
            "S": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "T": {"anyOf": [{"const": "a"}, {"const": "b"}]},
            "U": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
            "V": {"anyOf": [{"minLength": 1}, {"maxLength": 2}]}
          }}
        }"#;
        let counted = stats(document).unwrap();
        assert_eq!(counted.any_of.total, 4);
        assert_eq!(counted.any_of.nullable, 1);
        assert_eq!(counted.any_of.constants, 1);
        assert_eq!(counted.any_of.disjoint_types, 1);
        assert_eq!(counted.any_of.other, 1);
    }

    #[test]
    fn an_optional_nullable_property_is_counted() {
        let document = br#"{
          "openapi": "3.1.0", "paths": {},
          "components": {"schemas": {"S": {
            "type": "object",
            "required": ["a"],
            "properties": {
              "a": {"type": ["string", "null"]},
              "b": {"type": ["string", "null"]},
              "c": {"type": "string"}
            }
          }}}
        }"#;
        assert_eq!(stats(document).unwrap().optional_and_nullable, 1);
    }

    #[test]
    fn bounded_integers_are_told_apart_from_bare_ones() {
        let document = br#"{
          "openapi": "3.1.0", "paths": {},
          "components": {"schemas": {
            "A": {"type": "integer", "minimum": 0, "maximum": 100},
            "B": {"type": "integer"}
          }}
        }"#;
        let counted = stats(document).unwrap();
        assert_eq!(counted.integers, 2);
        assert_eq!(counted.bounded_integers, 1);
    }

    #[test]
    fn a_recursive_schema_has_a_finite_depth() {
        let document = br##"{
          "openapi": "3.1.0", "paths": {},
          "components": {"schemas": {"Node": {
            "type": "object",
            "properties": {"child": {"$ref": "#/components/schemas/Node"}}
          }}}
        }"##;
        assert!(stats(document).unwrap().max_schema_depth >= 2);
    }

    #[test]
    fn external_references_are_visible() {
        let document = br##"{
          "openapi": "3.1.0", "paths": {},
          "components": {"schemas": {
            "A": {"$ref": "other.json#/components/schemas/B"},
            "B": {"$ref": "#/components/schemas/A"}
          }}
        }"##;
        assert_eq!(stats(document).unwrap().external_refs, 1);
    }
}
