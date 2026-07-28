//! Structurally identical types collapse into one — but identity is the wire contract.
//!
//! Two types that *serialize* identically may still *deserialize* differently: one denying unknown
//! members and one ignoring them read the same bytes to different outcomes. So the merge key is the
//! whole contract minus its documentation, and the table it implements is the one in the plan:
//!
//! | may merge | must not merge |
//! | --- | --- |
//! | an inline shape with another inline shape | two types the document named — names are API, and they evolve independently |
//! | an inline shape with a named type, the inline use becoming a reference | types differing in any contract field, doc-invisible ones included |
//! | — | types the caller named explicitly, which is a declaration of identity |
//!
//! *Enforcement:* merging compares a canonical encoding, so "identical" is `==` on a value rather
//! than a judgement scattered through the code.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::lower::{Collapse, Provisional};
use super::{ContractKind, FieldContract, TypeIndex, TypeRef};
use crate::shape::ShapeKey;

/// Merge every group of identical contracts, repeatedly until nothing changes.
///
/// A second round is not paranoia: when two types merge, the types that referred to them separately
/// become identical too, and that only becomes visible once the first merge has been applied.
/// `lowered` and `collapses` are rewritten alongside the contracts: both name types by index, so a
/// merge that renumbers them would leave them pointing at whatever now sits where a merged type
/// used to.
pub(super) fn run(
    mut types: Vec<Provisional>,
    lowered: &mut BTreeMap<ShapeKey, TypeRef>,
    collapses: &mut Vec<Collapse>,
) -> Vec<Provisional> {
    // Each round strictly reduces the number of types, so this terminates; the bound is a
    // belt-and-braces guard against a fingerprint that is not stable under remapping.
    for _ in 0..types.len().saturating_add(1) {
        let merge = plan(&types);
        if merge.is_empty() {
            break;
        }
        types = apply(types, &merge, lowered, collapses);
    }
    // Two types that merged reported the same collapse at the same place, and the merge is what
    // makes that visible: one type now, so one finding. The kind is part of the identity — a
    // property can be both a presence collapse and `readOnly`, and those are two findings.
    collapses.sort_by(|left, right| {
        (left.owner, left.at.to_string(), left.kind as u8).cmp(&(
            right.owner,
            right.at.to_string(),
            right.kind as u8,
        ))
    });
    collapses.dedup_by(|left, right| left == right);
    types
}

/// Which types merge into which, by index.
fn plan(types: &[Provisional]) -> BTreeMap<usize, usize> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, contract) in types.iter().enumerate() {
        groups.entry(fingerprint(contract)).or_default().push(index);
    }

    let mut merge = BTreeMap::new();
    for group in groups.values() {
        if group.len() < 2 {
            continue;
        }
        let pinned: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&index| {
                types
                    .get(index)
                    .is_some_and(|it| it.declared || it.explicit)
            })
            .collect();
        let free: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&index| !pinned.contains(&index))
            .collect();
        // An inline shape prefers to become a reference to the type the document named; with no
        // named type in the group, the first occurrence survives, which is deterministic because
        // indices follow document order.
        let Some(survivor) = pinned.first().copied().or_else(|| free.first().copied()) else {
            continue;
        };
        for index in free {
            if index != survivor {
                merge.insert(index, survivor);
            }
        }
    }
    merge
}

/// Apply a merge plan: rewrite references, then drop what merged and renumber.
fn apply(
    mut types: Vec<Provisional>,
    merge: &BTreeMap<usize, usize>,
    lowered: &mut BTreeMap<ShapeKey, TypeRef>,
    collapses: &mut [Collapse],
) -> Vec<Provisional> {
    let mut renumbered = BTreeMap::new();
    let mut next = 0_u32;
    for index in 0..types.len() {
        if merge.contains_key(&index) {
            continue;
        }
        renumbered.insert(index, TypeIndex(next));
        next += 1;
    }

    let mut remap = BTreeMap::new();
    for index in 0..types.len() {
        let target = merge.get(&index).copied().unwrap_or(index);
        let Some(&to) = renumbered.get(&target) else {
            continue;
        };
        if let Ok(from) = u32::try_from(index) {
            remap.insert(TypeIndex(from), to);
        }
    }

    for contract in &mut types {
        for ty in references_mut(&mut contract.kind) {
            ty.remap(&remap);
        }
    }
    for ty in lowered.values_mut() {
        ty.remap(&remap);
    }
    for collapse in collapses {
        if let Some(&to) = remap.get(&collapse.owner) {
            collapse.owner = to;
        }
    }
    let mut survivors = Vec::with_capacity(renumbered.len());
    for (index, contract) in types.into_iter().enumerate() {
        if !merge.contains_key(&index) {
            survivors.push(contract);
        }
    }
    survivors
}

/// Every type reference a contract holds.
pub(super) fn references_mut(kind: &mut ContractKind) -> Vec<&mut TypeRef> {
    match kind {
        ContractKind::Struct { fields } => fields.iter_mut().map(|field| &mut field.ty).collect(),
        ContractKind::Enum { variants } => {
            variants.iter_mut().map(|variant| &mut variant.ty).collect()
        }
        ContractKind::StringEnum { .. } => Vec::new(),
        ContractKind::Newtype { inner } => vec![inner],
        ContractKind::Tuple { items } => items.iter_mut().collect(),
        ContractKind::Alias { target } => vec![target],
    }
}

/// The canonical encoding two contracts are compared by.
///
/// Documentation is excluded deliberately: two shapes that behave identically on the wire are one
/// type, and the surviving name keeps its own docs. Everything else is in, including the fields a
/// reader of the generated source cannot see.
fn fingerprint(contract: &Provisional) -> String {
    let mut out = String::new();
    let _ = write!(out, "{:?}|{:?}|", contract.tagging, contract.unknown_fields);
    match &contract.kind {
        ContractKind::Struct { fields } => {
            out.push_str("struct");
            for field in fields {
                field_print(&mut out, field);
            }
        }
        ContractKind::Enum { variants } => {
            out.push_str("enum");
            for variant in variants {
                let _ = write!(out, "|{}:{:?}", variant.rust_name, variant.ty);
            }
        }
        ContractKind::StringEnum { variants } => {
            out.push_str("strings");
            for variant in variants {
                let _ = write!(out, "|{}={}", variant.rust_name, variant.wire_name);
            }
        }
        ContractKind::Newtype { inner } => {
            let _ = write!(out, "newtype|{inner:?}");
        }
        ContractKind::Tuple { items } => {
            let _ = write!(out, "tuple|{items:?}");
        }
        ContractKind::Alias { target } => {
            let _ = write!(out, "alias|{target:?}");
        }
    }
    out
}

fn field_print(out: &mut String, field: &FieldContract) {
    let _ = write!(
        out,
        "|{}={}:{:?}:{:?}:{:?}:{}:{:?}",
        field.rust_name,
        field.wire_name,
        field.ty,
        field.presence,
        field.skip_serializing_if,
        field.flatten,
        field.default,
    );
}
