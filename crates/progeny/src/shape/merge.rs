//! Expanding `$ref` and `allOf` into a key, and merging that key's schemas into one view.
//!
//! The predecessor's "heroic `allOf` reconciliation" was compensation for a model that could not
//! hold both sides of a merge. Here it is a boring fold: union the properties, union the required
//! names, intersect the type sets and the enums, and where two parts give one property two
//! different schemas, merge *those* — which is the same operation one level down, so it needs no
//! separate rule.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Number, Value};

use super::{Docs, ShapeKey};
use crate::resolve::ResolvedDocument;
use crate::schema::{Schema, SchemaId, SchemaObject, TypeName};

/// How deep a chain of `$ref`/`allOf` expansion goes before the walk calls it a loop.
const MAX_PARTS: usize = 512;

/// The key of the shape written at `id`.
///
/// Follows `$ref` and flattens `allOf`, collecting the schemas that actually constrain the value.
/// A schema that only points somewhere — `{$ref: X}`, `{allOf: [X]}`, `{$ref: X, description: …}` —
/// contributes nothing of its own and so drops out of the key entirely, which is what makes a
/// reference position share a type with its target instead of copying it.
pub(crate) fn key_of(resolved: &ResolvedDocument, id: SchemaId) -> ShapeKey {
    let mut parts = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        if !seen.insert(current) || seen.len() > MAX_PARTS {
            continue;
        }
        match resolved.schema(current) {
            // `true` accepts anything, so it constrains nothing and adds nothing to a merge.
            Schema::Bool(true) => {}
            Schema::Bool(false) => parts.push(current),
            Schema::Object(object) => {
                if let Some(target) = resolved.follow(current) {
                    stack.push(target);
                }
                for &branch in object.all_of.iter().flatten() {
                    stack.push(branch);
                }
                if constrains(object) {
                    parts.push(current);
                }
            }
        }
    }
    if parts.is_empty() {
        // Nothing constrains the value: the key is the schema itself, and it classifies as "any".
        parts.push(id);
    }
    ShapeKey::new(parts)
}

/// The key of the shape all of these schemas describe together.
fn merged_key(resolved: &ResolvedDocument, ids: &[SchemaId]) -> ShapeKey {
    let mut parts = Vec::new();
    for &id in ids {
        parts.extend_from_slice(key_of(resolved, id).parts());
    }
    ShapeKey::new(parts)
}

/// Whether a schema says anything about the *value*, as opposed to about itself.
///
/// Annotations are excluded deliberately — see the module documentation in
/// [`super`]: a description beside a `$ref` must not fork a type.
fn constrains(object: &SchemaObject) -> bool {
    object.types.is_some()
        || object.enumeration.is_some()
        || object.constant.is_some()
        || object.any_of.is_some()
        || object.one_of.is_some()
        || object.not.is_some()
        || object.if_schema.is_some()
        || object.then_schema.is_some()
        || object.else_schema.is_some()
        || object.dependent_schemas.is_some()
        || object.properties.is_some()
        || object.pattern_properties.is_some()
        || object.additional_properties.is_some()
        || object.property_names.is_some()
        || object.items.is_some()
        || object.prefix_items.is_some()
        || object.contains.is_some()
        || object.unevaluated_items.is_some()
        || object.unevaluated_properties.is_some()
        || object.required.is_some()
        || object.dependent_required.is_some()
        || object.min_items.is_some()
        || object.max_items.is_some()
        || object.minimum.is_some()
        || object.exclusive_minimum.is_some()
        // Format-ish keywords decide which type a string becomes, so they constrain.
        || object.format.is_some()
        || object.content_encoding.is_some()
        || object.content_media_type.is_some()
        || object.content_schema.is_some()
        || object.discriminator.is_some()
}

/// A declared discriminator: which property names the variant, and what each name points at.
///
/// The mapping is carried as the document wrote it — a `$ref` string, or the bare component name
/// OpenAPI also allows — because turning it into a schema is resolution's job and reporting what a
/// mapping *said* when it names nothing is the diagnostic's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Discriminated {
    pub(crate) property: String,
    pub(crate) mapping: BTreeMap<String, String>,
}

/// Every part of a key, folded into one statement about the value.
#[derive(Debug, Default)]
pub(crate) struct View {
    pub(crate) types: Option<BTreeSet<TypeName>>,
    pub(crate) enumeration: Option<Vec<Value>>,
    pub(crate) constant: Option<Value>,
    /// Every declared property, with the key its schemas merge into.
    pub(crate) properties: BTreeMap<String, ShapeKey>,
    pub(crate) required: BTreeSet<String>,
    /// Per-property annotations, read from the position that carries them.
    pub(crate) property_docs: BTreeMap<String, Docs>,
    pub(crate) property_defaults: BTreeMap<String, Value>,
    pub(crate) read_only: BTreeSet<String>,
    pub(crate) write_only: BTreeSet<String>,
    pub(crate) additional: Option<ShapeKey>,
    pub(crate) additional_denied: bool,
    /// The value schema of a `patternProperties` whose patterns all agree, when there is one.
    pub(crate) uniform_pattern: Option<ShapeKey>,
    pub(crate) items: Option<ShapeKey>,
    pub(crate) prefix_items: Option<Vec<ShapeKey>>,
    pub(crate) min_items: Option<u32>,
    pub(crate) max_items: Option<u32>,
    /// Whether some part says the value cannot be negative.
    pub(crate) unsigned: bool,
    pub(crate) format: Option<String>,
    pub(crate) content_encoding: Option<String>,
    pub(crate) content_media_type: Option<String>,
    pub(crate) union: Option<Vec<ShapeKey>>,
    /// The declared discriminator, when there is one.
    pub(crate) discriminator: Option<Discriminated>,
    /// Set when two parts each declare a union: an intersection of two unions has no faithful
    /// Rust type, and pretending otherwise is the "wild union" failure mode.
    pub(crate) unions_collide: bool,
    pub(crate) docs: Docs,
    /// Keywords progeny holds but does not interpret, named for the diagnostic.
    pub(crate) uninterpreted: Vec<&'static str>,
    /// Set when the parts cannot all hold at once.
    pub(crate) conflict: Option<String>,
    /// Set when a part accepts nothing at all (`false`).
    pub(crate) impossible: bool,
    /// `type` spellings that are not one of the seven, kept for the diagnostic.
    pub(crate) unknown_types: Vec<String>,
}

/// Fold every part of a key into one view.
pub(crate) fn view(resolved: &ResolvedDocument, key: &ShapeKey) -> View {
    let mut view = View::default();
    let mut properties: BTreeMap<String, Vec<SchemaId>> = BTreeMap::new();
    let mut additional: Vec<SchemaId> = Vec::new();
    let mut items: Vec<SchemaId> = Vec::new();
    let mut patterns: Vec<SchemaId> = Vec::new();

    for &part in key.parts() {
        let object = match resolved.schema(part) {
            Schema::Bool(false) => {
                view.impossible = true;
                continue;
            }
            Schema::Bool(true) => continue,
            Schema::Object(object) => object,
        };
        fold_types(&mut view, object);
        fold_values(&mut view, object, resolved);
        fold_object(
            &mut view,
            object,
            resolved,
            &mut properties,
            &mut additional,
            &mut patterns,
        );
        fold_array(&mut view, object, resolved, &mut items);
        fold_annotations(&mut view, object);
        note_uninterpreted(&mut view, object);
    }

    for (name, ids) in properties {
        view.properties.insert(name, merged_key(resolved, &ids));
    }
    if !additional.is_empty() {
        view.additional = Some(merged_key(resolved, &additional));
    }
    if !items.is_empty() {
        view.items = Some(merged_key(resolved, &items));
    }
    if !patterns.is_empty() {
        // Uniform means every pattern names the same value schema; anything else has no single
        // element type and is reported rather than guessed at.
        let first = patterns.first().map_or_else(
            || ShapeKey::new(Vec::new()),
            |&id| merged_key(resolved, &[id]),
        );
        let uniform = patterns
            .iter()
            .all(|&id| merged_key(resolved, &[id]) == first);
        if uniform {
            view.uniform_pattern = Some(first);
        } else {
            view.uninterpreted.push("patternProperties");
        }
    }
    view
}

fn fold_types(view: &mut View, object: &SchemaObject) {
    // Once the parts have been found irreconcilable there is nothing left to intersect, and
    // continuing would report the same conflict again against an already-empty set.
    if view.conflict.is_some() {
        return;
    }
    let Some(declared) = &object.types else {
        return;
    };
    let mut names = BTreeSet::new();
    for name in declared.iter() {
        if let TypeName::Other(spelling) = name {
            view.unknown_types.push(spelling.clone());
            continue;
        }
        names.insert(name.clone());
    }
    if names.is_empty() {
        return;
    }
    match &mut view.types {
        Some(existing) => {
            let intersection = intersect(existing, &names);
            if intersection.is_empty() {
                view.conflict = Some(format!(
                    "`type` is declared as {} in one branch and {} in another, and no value \
                     satisfies both",
                    render(existing),
                    render(&names)
                ));
            }
            *existing = intersection;
        }
        None => view.types = Some(names),
    }
}

/// The types a value must satisfy when it has to satisfy both of these sets.
///
/// Set intersection, plus the one containment JSON Schema's type names hide: `integer` is a subset
/// of `number`, so `allOf: [{type: number}, {type: integer}]` is an integer rather than a
/// contradiction. Treating the names as opaque would report a conflict where real documents —
/// `github`'s workflow-job payloads among them — say something perfectly consistent.
fn intersect(left: &BTreeSet<TypeName>, right: &BTreeSet<TypeName>) -> BTreeSet<TypeName> {
    let mut both: BTreeSet<TypeName> = left.intersection(right).cloned().collect();
    let narrows = |wide: &BTreeSet<TypeName>, narrow: &BTreeSet<TypeName>| {
        wide.contains(&TypeName::Number) && narrow.contains(&TypeName::Integer)
    };
    if narrows(left, right) || narrows(right, left) {
        both.insert(TypeName::Integer);
    }
    both
}

fn render(names: &BTreeSet<TypeName>) -> String {
    let rendered: Vec<&str> = names.iter().map(TypeName::as_str).collect();
    rendered.join("/")
}

fn fold_values(view: &mut View, object: &SchemaObject, resolved: &ResolvedDocument) {
    if let Some(values) = &object.enumeration {
        match &mut view.enumeration {
            Some(existing) => existing.retain(|value| values.contains(value)),
            None => view.enumeration = Some(values.clone()),
        }
    }
    if let Some(value) = &object.constant
        && view.constant.is_none()
    {
        view.constant = Some(value.clone());
    }
    if let Some(discriminator) = &object.discriminator
        && view.discriminator.is_none()
        && let Some(property) = discriminator.property_name.clone()
    {
        view.discriminator = Some(Discriminated {
            property,
            mapping: discriminator.mapping.clone().unwrap_or_default(),
        });
    }
    for branches in [&object.one_of, &object.any_of] {
        let Some(branches) = branches else {
            continue;
        };
        if view.union.is_some() {
            view.unions_collide = true;
            continue;
        }
        view.union = Some(branches.iter().map(|&id| key_of(resolved, id)).collect());
    }
    if object.minimum.as_ref().is_some_and(non_negative)
        || object.exclusive_minimum.as_ref().is_some_and(non_negative)
    {
        view.unsigned = true;
    }
}

/// Whether a bound says the value cannot be negative.
///
/// `-0` and a bound too large for `f64` both answer "no", which is the safe direction: a signed
/// integer holds every value an unsigned one does.
fn non_negative(bound: &Number) -> bool {
    bound.as_f64().is_some_and(|value| value >= 0.0)
}

fn fold_object(
    view: &mut View,
    object: &SchemaObject,
    resolved: &ResolvedDocument,
    properties: &mut BTreeMap<String, Vec<SchemaId>>,
    additional: &mut Vec<SchemaId>,
    patterns: &mut Vec<SchemaId>,
) {
    for (name, &id) in object.properties.iter().flatten() {
        properties.entry(name.clone()).or_default().push(id);
        property_annotations(view, resolved, name, id);
    }
    for name in object.required.iter().flatten() {
        view.required.insert(name.clone());
    }
    if let Some(id) = object.additional_properties {
        match resolved.schema(id) {
            Schema::Bool(false) => view.additional_denied = true,
            // `true` is what an absent `additionalProperties` already means.
            Schema::Bool(true) => {}
            Schema::Object(_) => additional.push(id),
        }
    }
    for (_, &id) in object.pattern_properties.iter().flatten() {
        patterns.push(id);
    }
}

fn fold_array(
    view: &mut View,
    object: &SchemaObject,
    resolved: &ResolvedDocument,
    items: &mut Vec<SchemaId>,
) {
    if let Some(id) = object.items {
        items.push(id);
    }
    if let Some(prefix) = &object.prefix_items
        && view.prefix_items.is_none()
    {
        view.prefix_items = Some(prefix.iter().map(|&id| key_of(resolved, id)).collect());
    }
    if let Some(count) = object.min_items.as_ref().and_then(as_count) {
        view.min_items = Some(view.min_items.map_or(count, |seen| seen.max(count)));
    }
    if let Some(count) = object.max_items.as_ref().and_then(as_count) {
        view.max_items = Some(view.max_items.map_or(count, |seen| seen.min(count)));
    }
}

fn as_count(number: &Number) -> Option<u32> {
    u32::try_from(number.as_u64()?).ok()
}

fn fold_annotations(view: &mut View, object: &SchemaObject) {
    if view.docs.title.is_none() {
        view.docs.title.clone_from(&object.title);
    }
    if view.docs.description.is_none() {
        view.docs.description.clone_from(&object.description);
    }
    view.docs.deprecated |= object.deprecated.unwrap_or(false);
    for (slot, value) in [
        (&mut view.format, &object.format),
        (&mut view.content_encoding, &object.content_encoding),
        (&mut view.content_media_type, &object.content_media_type),
    ] {
        if slot.is_none() {
            slot.clone_from(value);
        }
    }
}

/// Note the keywords progeny holds losslessly but does not turn into a type.
///
/// Every one of these narrows which values are *valid* without changing which values are
/// *representable*, so ignoring them yields a type that accepts a superset of what the document
/// allows — the same direction as tolerating unknown fields, and the only sound direction
/// available. The diagnostic is what keeps it from being silent.
fn note_uninterpreted(view: &mut View, object: &SchemaObject) {
    for (keyword, present) in [
        ("not", object.not.is_some()),
        ("if", object.if_schema.is_some()),
        ("then", object.then_schema.is_some()),
        ("else", object.else_schema.is_some()),
        ("dependentSchemas", object.dependent_schemas.is_some()),
        ("dependentRequired", object.dependent_required.is_some()),
        ("propertyNames", object.property_names.is_some()),
        ("contains", object.contains.is_some()),
        ("unevaluatedItems", object.unevaluated_items.is_some()),
        (
            "unevaluatedProperties",
            object.unevaluated_properties.is_some(),
        ),
    ] {
        if present && !view.uninterpreted.contains(&keyword) {
            view.uninterpreted.push(keyword);
        }
    }
}

/// Read a property's own annotations, which stay with the field rather than with its type.
///
/// Read from the schema *at the property position* rather than from the type it resolves to, which
/// is what makes `{$ref: Pet, description: "the owner's pet"}` document the field without forking
/// `Pet` into a second type.
fn property_annotations(view: &mut View, resolved: &ResolvedDocument, name: &str, id: SchemaId) {
    let Schema::Object(property) = resolved.schema(id) else {
        return;
    };
    let docs = Docs {
        title: property.title.clone(),
        description: property.description.clone(),
        deprecated: property.deprecated.unwrap_or(false),
    };
    if !docs.is_empty() {
        view.property_docs.entry(name.to_owned()).or_insert(docs);
    }
    if let Some(default) = &property.default {
        view.property_defaults
            .entry(name.to_owned())
            .or_insert_with(|| default.clone());
    }
    if property.read_only.unwrap_or(false) {
        view.read_only.insert(name.to_owned());
    }
    if property.write_only.unwrap_or(false) {
        view.write_only.insert(name.to_owned());
    }
}
