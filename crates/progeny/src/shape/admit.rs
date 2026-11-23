//! Which unions read by the kind of each payload and then by tag are typed.
//!
//! [`super::classify`] finds such a union where it would otherwise be arbitrary JSON, and leaves it
//! arbitrary JSON, so the document's other shapes and tags are classified and settled exactly as
//! they would be without it.
//! A union is then admitted only where reading it so changes none of them: its objects are
//! structs the settled graph already holds, intact and with their tag, and its other branches
//! hold scalar data, which no tag decision reads.
//! Every union is judged against the same settled graph, so which are admitted does not depend on
//! the order they are judged in.

use std::collections::{BTreeMap, BTreeSet};

use super::classify::{self, ComponentNames, Known};
use super::{Shape, ShapeKey, ShapeRef, Shapes, Union};
use crate::config::IntegerWidth;
use crate::diag::Ctx;
use crate::resolve::ResolvedDocument;

/// How far through aliases and lists a branch of another kind is followed before it counts as
/// more than scalar data.
const MAX_DATA_DEPTH: usize = 16;

/// What settling the document's tags decided, which an admitted union must leave as it is.
pub(super) struct Settled {
    /// The variant types a consumed tag took a property off.
    pub(super) stripped: BTreeSet<ShapeKey>,
    /// The shapes a tagged union's variants declare their tag property as, which settling
    /// checked are strings ([`tag_fields`]).
    pub(super) tag_fields: BTreeSet<ShapeKey>,
}

/// The unions in `found` that reading by kind and then tag changes nothing else for.
///
/// Each object branch has to be a struct the settled graph holds, with no union flattened into it,
/// still declaring the tag, and with no property taken off by a settled union, so the union
/// carries its tag over types every other position reads the same way.
/// Each other branch has to be scalar data ([`scalar_data`]), which no tag is settled by.
/// And the union must not stand where settling read a tag's declaration, which accepted arbitrary
/// JSON there and might not accept the union.
pub(super) fn admitted(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    found: &BTreeMap<ShapeKey, Union>,
    settled: &Settled,
    component_names: &ComponentNames,
    integers: IntegerWidth,
) -> BTreeSet<ShapeKey> {
    found
        .iter()
        .filter(|(key, union)| {
            !settled.tag_fields.contains(*key)
                && intact_objects(shapes, union, &settled.stripped)
                && union
                    .tag
                    .iter()
                    .flat_map(|tag| &tag.kinds)
                    .all(|kind| scalar_data(resolved, shapes, kind, component_names, integers, 0))
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Whether every object branch is a plain struct of the settled graph that kept every property,
/// the tag among them.
fn intact_objects(shapes: &Shapes, union: &Union, stripped: &BTreeSet<ShapeKey>) -> bool {
    let Some(tag) = &union.tag else {
        return false;
    };
    union.variants.iter().all(|variant| {
        let ShapeRef::Key(key) = &variant.shape else {
            return false;
        };
        let Some(Shape::Struct(structure)) = shapes.get(key) else {
            return false;
        };
        structure.flattened.is_none()
            && !stripped.contains(key)
            && structure
                .fields
                .iter()
                .any(|field| field.wire == tag.property)
    })
}

/// Whether a branch is generated as scalar data: a scalar, a string enum, or lists and tuples of
/// those, through aliases and `Option`s.
///
/// A branch nothing else reaches has no shape yet, so it is classified on its own, with its
/// diagnostics left out: an admitted union reaches it again when it is typed.
fn scalar_data(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    reference: &ShapeRef,
    component_names: &ComponentNames,
    integers: IntegerWidth,
    depth: usize,
) -> bool {
    if depth > MAX_DATA_DEPTH {
        return false;
    }
    let shape = match reference {
        ShapeRef::Inline(shape) => (**shape).clone(),
        ShapeRef::Key(key) => shapes.get(key).cloned().unwrap_or_else(|| {
            let mut known = Known {
                component_names,
                admitted: &BTreeSet::new(),
                found: BTreeMap::new(),
            };
            classify::key(resolved, key, &mut known, integers, &mut Ctx::new())
        }),
    };
    let data = |inner: &ShapeRef| {
        scalar_data(
            resolved,
            shapes,
            inner,
            component_names,
            integers,
            depth.saturating_add(1),
        )
    };
    match &shape {
        Shape::Scalar(_) | Shape::Format(_) | Shape::StringEnum(_) => true,
        Shape::Alias(inner)
        | Shape::Optional(inner)
        | Shape::Array { item: Some(inner) }
        | Shape::FixedArray { item: inner, .. } => data(inner),
        Shape::Tuple { items } => items.iter().all(data),
        Shape::Null
        | Shape::Struct(_)
        | Shape::Map { .. }
        | Shape::Array { item: None }
        | Shape::Union(_)
        | Shape::Keyed(_)
        | Shape::Any => false,
    }
}

/// The shapes a tagged union's variant declares its tag property as, followed through aliases and
/// `Option`s the way settling reads them.
///
/// Settling refuses a tag declared as anything but a string, and arbitrary JSON passes, so a union
/// standing there now would be read as something else once typed.
pub(super) fn tag_fields(shapes: &Shapes) -> BTreeSet<ShapeKey> {
    let mut fields = BTreeSet::new();
    for (_, shape) in shapes.entries() {
        let union = match shape {
            Shape::Union(union) => union,
            Shape::Optional(ShapeRef::Inline(inner)) => match inner.as_ref() {
                Shape::Union(union) => union,
                _ => continue,
            },
            _ => continue,
        };
        let Some(tag) = &union.tag else {
            continue;
        };
        for variant in &union.variants {
            let ShapeRef::Key(key) = &variant.shape else {
                continue;
            };
            let Some(Shape::Struct(structure)) = shapes.get(key) else {
                continue;
            };
            for field in structure
                .fields
                .iter()
                .filter(|field| field.wire == tag.property)
            {
                let mut reference = Some(&field.shape);
                while let Some(current) = reference {
                    let shape = match current {
                        ShapeRef::Key(key) if !fields.insert(key.clone()) => break,
                        ShapeRef::Key(key) => shapes.get(key),
                        ShapeRef::Inline(shape) => Some(shape.as_ref()),
                    };
                    reference = match shape {
                        Some(Shape::Alias(inner) | Shape::Optional(inner)) => Some(inner),
                        _ => None,
                    };
                }
            }
        }
    }
    fields
}
