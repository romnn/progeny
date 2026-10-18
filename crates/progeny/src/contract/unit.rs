//! A branch of a tagged union that carries nothing but its tag is a unit variant.
//!
//! Lowering gives every branch a payload type, because whether that type is empty in every form
//! depends on the direction it travels, which is known only once the type graph exists.
//! A read form keeps the members its description did not declare in a capture map, so a branch
//! read only from responses under lenient decoding has something to hold, and it stays a struct.
//! Everywhere else an empty struct that ignores undeclared members reads and writes exactly what
//! a unit variant does, and the variant drops it.
//!
//! The struct is removed along with its last use, so no empty type is left behind under one
//! branch's title.

use std::collections::{BTreeMap, BTreeSet};

use super::lower::{Collapse, Provisional};
use super::reach::Reach;
use super::{ContractKind, TypeIndex, TypeRef};
use crate::config::{Config, UnknownFields};
use crate::shape::ShapeKey;

/// Make every tag-only branch a unit variant, and drop the empty types that held them.
///
/// `reach` is each type's direction under lenient decoding, and `None` under strict decoding,
/// where no type has a read form.
/// It is renumbered with the types, as `by_shape` and `collapses` are.
pub(super) fn run(
    types: Vec<Provisional>,
    config: &Config,
    by_shape: &mut BTreeMap<ShapeKey, TypeRef>,
    collapses: &mut Vec<Collapse>,
    tag_branches: &BTreeSet<ShapeKey>,
    reach: Option<&mut Vec<Reach>>,
) -> Vec<Provisional> {
    let units = units(
        &types,
        config,
        by_shape,
        tag_branches,
        reach.as_deref().map(Vec::as_slice),
    );
    if units.is_empty() {
        return types;
    }
    remove(types, &units, by_shape, collapses, reach)
}

/// The types that exist only to be a tag-only branch's empty payload.
fn units(
    types: &[Provisional],
    config: &Config,
    by_shape: &BTreeMap<ShapeKey, TypeRef>,
    tag_branches: &BTreeSet<ShapeKey>,
    reach: Option<&[Reach]>,
) -> BTreeSet<TypeIndex> {
    // Candidates: empty structs that ignore undeclared members, that no read form gives a capture
    // map, and that neither the document nor the configuration talks about by name.
    let mut units: BTreeSet<TypeIndex> = types
        .iter()
        .enumerate()
        .filter(|(index, contract)| {
            let read_only = reach
                .and_then(|reach| reach.get(*index))
                .is_some_and(|at| at.response && !at.request);
            matches!(&contract.kind, ContractKind::Struct { fields } if fields.is_empty())
                && contract.unknown_fields == UnknownFields::Ignore
                && contract.component.is_none()
                && !config.configures_type(None, &contract.origin.to_string())
                && !read_only
        })
        .filter_map(|(index, _)| u32::try_from(index).ok().map(TypeIndex))
        .collect();

    // A candidate held anywhere but as a consumed-tag variant's whole payload is a type in its
    // own right, so it stays.
    for contract in types {
        let mut reached = Vec::new();
        match &contract.kind {
            // A tagged union holds types only in its variants.
            ContractKind::TaggedEnum { variants, .. } => {
                for variant in variants {
                    if !matches!(variant.ty, TypeRef::Named(_)) {
                        variant.ty.named(&mut reached);
                    }
                }
            }
            other => {
                for ty in other.references() {
                    ty.named(&mut reached);
                }
            }
        }
        for index in reached {
            units.remove(&index);
        }
    }
    // The same for a shape that lowered to it: only a consumed-tag branch may.
    for (key, ty) in by_shape {
        let mut reached = Vec::new();
        ty.named(&mut reached);
        let branch = tag_branches.contains(key) && matches!(ty, TypeRef::Named(_));
        if !branch {
            for index in reached {
                units.remove(&index);
            }
        }
    }
    units
}

/// Turn each use of a unit payload into a unit variant, then drop the payloads and renumber.
fn remove(
    mut types: Vec<Provisional>,
    units: &BTreeSet<TypeIndex>,
    by_shape: &mut BTreeMap<ShapeKey, TypeRef>,
    collapses: &mut Vec<Collapse>,
    reach: Option<&mut Vec<Reach>>,
) -> Vec<Provisional> {
    let is_unit = |ty: &TypeRef| matches!(ty, TypeRef::Named(index) if units.contains(index));
    for contract in &mut types {
        if let ContractKind::TaggedEnum { variants, .. } = &mut contract.kind {
            for variant in variants.iter_mut().filter(|variant| is_unit(&variant.ty)) {
                variant.ty = TypeRef::Unit;
            }
        }
    }
    for ty in by_shape.values_mut().filter(|ty| is_unit(ty)) {
        *ty = TypeRef::Unit;
    }

    // Every surviving type moves down past the units before it.
    let mut remap = BTreeMap::new();
    let mut next = 0_u32;
    for index in 0..types.len() {
        let Ok(from) = u32::try_from(index).map(TypeIndex) else {
            continue;
        };
        if !units.contains(&from) {
            remap.insert(from, TypeIndex(next));
            next += 1;
        }
    }
    for contract in &mut types {
        for ty in super::dedup::references_mut(&mut contract.kind) {
            ty.remap(&remap);
        }
    }
    for ty in by_shape.values_mut() {
        ty.remap(&remap);
    }
    collapses.retain(|collapse| !units.contains(&collapse.owner));
    for collapse in collapses.iter_mut() {
        if let Some(&to) = remap.get(&collapse.owner) {
            collapse.owner = to;
        }
    }
    let kept = |index: usize| {
        u32::try_from(index)
            .map(TypeIndex)
            .is_ok_and(|index| !units.contains(&index))
    };
    if let Some(reach) = reach {
        let mut index = 0;
        reach.retain(|_| {
            let keep = kept(index);
            index += 1;
            keep
        });
    }
    types
        .into_iter()
        .enumerate()
        .filter(|(index, _)| kept(*index))
        .map(|(_, contract)| contract)
        .collect()
}
