//! Which unions read by the kind of each payload and then by tag are typed.
//!
//! [`super::classify`] finds such a union where it would otherwise be arbitrary JSON, and leaves it
//! arbitrary JSON, so the document's other shapes and tags are classified and settled exactly as
//! they would be without it.
//! A union is then admitted only where reading it so changes none of them: settling never read a
//! tag's declaration through it, its objects are structs the settled graph already holds, intact
//! and with their tag, and its other branches hold scalar data, which no tag decision reads.
//! Every union is judged against the same settled graph, so which are admitted does not depend on
//! the order they are judged in.

use std::collections::{BTreeMap, BTreeSet};

use super::classify::{self, Known};
use super::discriminate::{Settlement, with_union};
use super::{Discovered, Shape, ShapeKey, ShapeRef, Shapes, Union};
use crate::config::IntegerWidth;
use crate::diag::{Ctx, Diagnostic};
use crate::resolve::ResolvedDocument;

/// How far through aliases and lists a branch of another kind is followed before it counts as
/// more than scalar data.
const MAX_DATA_DEPTH: usize = 16;

/// What admitting unions read by kind and then by tag adds to the settled graph.
#[derive(Debug)]
pub(super) struct Admission {
    /// The shape each admitted union's key takes: the arbitrary JSON it was classified as, typed
    /// as the union.
    pub(super) unions: BTreeMap<ShapeKey, Shape>,
    /// The scalar data only admitted unions reach, each classified once.
    pub(super) data: BTreeMap<ShapeKey, Classified>,
}

/// A shape classified outside the work list, with what classifying it reported.
#[derive(Debug)]
pub(super) struct Classified {
    pub(super) shape: Shape,
    pub(super) reports: Vec<Diagnostic>,
}

/// Admit the unions found readable by kind and then by tag that reading so changes nothing else
/// for.
///
/// A union is admitted where settling never followed it to read how a variant declares its tag,
/// each object branch is a struct the settled graph holds intact ([`intact_objects`]), and each
/// other branch is scalar data ([`Probe::scalar_data`]), which no tag is settled by.
pub(super) fn admit(
    resolved: &ResolvedDocument,
    discovered: &Discovered,
    settlement: &Settlement,
    integers: IntegerWidth,
) -> Admission {
    let mut probe = Probe {
        resolved,
        shapes: &discovered.shapes,
        known: Known {
            component_names: &discovered.component_names,
            found: BTreeMap::new(),
        },
        integers,
        classified: BTreeMap::new(),
    };
    let mut unions = BTreeMap::new();
    let mut reached = BTreeSet::new();
    for (key, union) in &discovered.found {
        if settlement.tag_reads.contains(key) || !intact_objects(probe.shapes, union, settlement) {
            continue;
        }
        let mut data = BTreeSet::new();
        let scalar = union
            .tag
            .iter()
            .flat_map(|tag| &tag.kinds)
            .all(|kind| probe.scalar_data(kind, 0, &mut data));
        if !scalar {
            continue;
        }
        // The union is arbitrary JSON inside every `Option` it was classified in.
        let Some(fallback) = probe.shapes.get(key) else {
            continue;
        };
        unions.insert(key.clone(), with_union(fallback, union.clone()));
        reached.append(&mut data);
    }
    let mut data = probe.classified;
    data.retain(|key, _| reached.contains(key));
    Admission { unions, data }
}

/// Whether every object branch is a plain struct of the settled graph that kept every property,
/// the tag among them.
fn intact_objects(shapes: &Shapes, union: &Union, settlement: &Settlement) -> bool {
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
            && !settlement.strips(key)
            && structure
                .fields
                .iter()
                .any(|field| field.wire == tag.property)
    })
}

/// The settled graph, and the shapes classified for branches it does not hold.
struct Probe<'a> {
    resolved: &'a ResolvedDocument,
    shapes: &'a Shapes,
    known: Known<'a>,
    integers: IntegerWidth,
    /// Each key the settled graph does not hold, classified once whichever union reaches it.
    classified: BTreeMap<ShapeKey, Classified>,
}

impl Probe<'_> {
    /// Whether a branch is generated as scalar data: a scalar, a string enum, or lists and tuples
    /// of those, through aliases and `Option`s.
    ///
    /// A key the settled graph does not hold is classified on its own and added to `reached`, the
    /// shapes the union would add to the graph.
    fn scalar_data(
        &mut self,
        reference: &ShapeRef,
        depth: usize,
        reached: &mut BTreeSet<ShapeKey>,
    ) -> bool {
        if depth > MAX_DATA_DEPTH {
            return false;
        }
        let shape = match reference {
            ShapeRef::Inline(shape) => (**shape).clone(),
            ShapeRef::Key(key) => match self.shapes.get(key) {
                Some(shape) => shape.clone(),
                None => {
                    reached.insert(key.clone());
                    self.classify(key).clone()
                }
            },
        };
        let mut data = |inner: &ShapeRef| self.scalar_data(inner, depth.saturating_add(1), reached);
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

    /// What `key` classifies as, classifying it the first time it is asked for.
    fn classify(&mut self, key: &ShapeKey) -> &Shape {
        let classified = self.classified.entry(key.clone()).or_insert_with(|| {
            let mut reports = Ctx::recording();
            let shape = classify::key(
                self.resolved,
                key,
                &mut self.known,
                self.integers,
                &mut reports,
            );
            Classified {
                shape,
                reports: reports.into_diagnostics(),
            }
        });
        &classified.shape
    }
}
