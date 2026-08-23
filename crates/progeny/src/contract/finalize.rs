//! The last phase: cycles broken, derives ruled on, serde strategy decided, records frozen.
//!
//! Nothing runs after this. That is the point: the two historical bugs both needed a phase that
//! could change a serde-relevant fact *after* eligibility had been computed, and there is no such
//! phase here — [`run`] consumes the provisional contracts and the caller's configuration together
//! and returns records with private fields.

use std::collections::{BTreeMap, BTreeSet};

use super::dedup::references_mut;
use super::lower::Provisional;
use super::{ContractKind, Contracts, DeserStrategy, Format, TypeContract, TypeIndex, TypeRef};
use crate::config::{Config, Derive, MapKind, SerdeImpl, UnknownFields};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, RejectError, RejectKind};
use crate::schema::cycles::Sccs;

/// The derives every generated type gets, whatever the caller asks for.
///
/// Two, deliberately. Each derive is function bodies the consumer compiles, and compile cost is a
/// property of the product rather than of progeny; anything beyond `Clone` and `Debug` is something
/// a caller can ask for by name.
pub(crate) const BASE: [Derive; 2] = [Derive::Clone, Derive::Debug];

/// Freeze the provisional contracts.
///
/// # Errors
///
/// Returns a rejection when the caller asked, by name, for a derive a type cannot have. A
/// crate-wide request is a preference and gets skipped with a diagnostic; a named one is a stated
/// intent that cannot be honoured, and quietly ignoring it would be the forbidden failure mode
/// applied to the configuration instead of to the document.
pub(super) fn run(
    mut types: Vec<Provisional>,
    config: &Config,
    ctx: &mut Ctx,
) -> Result<Contracts, RejectError> {
    promote_recursive_aliases(&mut types);
    break_cycles(&mut types);

    let supported = supported_derives(&types, config);
    let mut frozen = Vec::with_capacity(types.len());
    for (index, contract) in types.iter().enumerate() {
        let available = supported.get(index).cloned().unwrap_or_default();
        let requested =
            config.derives_for(contract.component.as_deref(), &contract.origin.to_string());
        let explicit = config.type_derives.keys().any(|key| matches(contract, key));

        let mut derives: Vec<Derive> = BASE.to_vec();
        for &derive in requested {
            if available.contains(&derive) {
                if !derives.contains(&derive) {
                    derives.push(derive);
                }
                continue;
            }
            if explicit {
                return Err(RejectError::new(
                    RejectKind::UnsatisfiableConfig,
                    format!(
                        "`{}` cannot derive `{}`, and the configuration asks for it by name",
                        contract.rust_name,
                        derive.name()
                    ),
                )
                .at(contract.origin.clone()));
            }
            ctx.report(Diagnostic::new(
                BreakageClass::UnsatisfiableDerive,
                Action::Warn,
                contract.origin.clone(),
                format!(
                    "the configuration asks every type to derive `{}`; this one cannot, so it was \
                     left off",
                    derive.name()
                ),
            ));
        }
        derives.sort_unstable();
        derives.dedup();

        frozen.push(TypeContract {
            rust_name: contract.rust_name.clone(),
            docs: contract.docs.clone(),
            deser: decide(&contract.kind, contract.unknown_fields, config),
            kind: contract.kind.clone(),
            unknown_fields: contract.unknown_fields,
            derives,
            origin: contract.origin.clone(),
        });
    }
    Ok(Contracts {
        types: frozen,
        by_shape: BTreeMap::new(),
        collapses: Vec::new(),
    })
}

/// Whether a configuration key names this type, under the one [`Address`] grammar.
fn matches(contract: &Provisional, key: &str) -> bool {
    crate::config::Address::parse(key)
        .names(contract.component.as_deref(), &contract.origin.to_string())
}

/// **The eligibility function.** Exhaustive, with no wildcard arm.
///
/// Adding a variant to [`ContractKind`] or [`UnknownFields`] breaks this function's compilation
/// until someone rules on it, which is the mechanism behind "a new serde option fails to compile
/// until it has been ruled on". The rulings themselves come from measurement (`04-render.md`): a
/// struct's derive expands to 9 function bodies and a fieldless enum's to 8 and climbing, while an
/// untagged enum's expands to 1 — so converting a union would *cost* compile time rather than save
/// it, and unions stay derived on purpose.
fn decide(kind: &ContractKind, unknown_fields: UnknownFields, config: &Config) -> DeserStrategy {
    match config.serde_impl {
        // The escape hatch. Every corpus document is generated both ways from stage 8 on, because
        // an escape hatch nobody runs is not one — the first corpus run in the other mode failed on
        // all eight tier documents. One kind is exempt from it: a carried-tag enum has no derive
        // encoding at all — see [`DeserStrategy::HandWrittenCarriedTag`] — so it takes the
        // hand-written path under both configurations, and the differential harness compares two
        // renderings that are identical there by construction.
        SerdeImpl::DeriveAlways => match kind {
            ContractKind::CarriedTagEnum { .. } => DeserStrategy::HandWrittenCarriedTag,
            ContractKind::Struct { .. }
            | ContractKind::Enum { .. }
            | ContractKind::TaggedEnum { .. }
            | ContractKind::StringEnum { .. }
            | ContractKind::Newtype { .. }
            | ContractKind::Tuple { .. }
            | ContractKind::Alias { .. } => DeserStrategy::Derive,
        },
        SerdeImpl::HandWrittenWhereEligible => match kind {
            ContractKind::StringEnum { .. } => DeserStrategy::HandWrittenFieldless,
            ContractKind::Struct { fields } => match unknown_fields {
                // `flatten` and the buffered implementation are not reconciled: the buffered
                // deserializer assigns members by name, and a flattened member claims whatever is
                // left, which is a second pass it does not have. Ruled to the derive rather than
                // left to chance — and note that `flatten` with `Deny` cannot even be asked for,
                // because capturing and denying are two arms of one enum.
                UnknownFields::Capture => DeserStrategy::Derive,
                UnknownFields::Ignore | UnknownFields::Deny => {
                    if fields.iter().any(|field| field.flatten) {
                        DeserStrategy::Derive
                    } else {
                        DeserStrategy::HandWrittenBuffered {
                            deny_unknown: unknown_fields == UnknownFields::Deny,
                        }
                    }
                }
            },
            // Data-carrying enums, wrappers, tuples and aliases: all derived in v1. A tagged
            // union could not take the buffered path even if the measurement changed: consuming
            // the tag means one member is gone before assignment starts, a notion the buffered
            // deserializer does not have.
            ContractKind::Enum { .. }
            | ContractKind::TaggedEnum { .. }
            | ContractKind::Newtype { .. }
            | ContractKind::Tuple { .. }
            | ContractKind::Alias { .. } => DeserStrategy::Derive,
            ContractKind::CarriedTagEnum { .. } => DeserStrategy::HandWrittenCarriedTag,
        },
    }
}

/// A type alias cannot be recursive at all in Rust — not even through a `Vec`, which would be fine
/// for a struct. An alias caught in a cycle of aliases becomes a newtype, which is the same wire
/// format with a name Rust can expand.
fn promote_recursive_aliases(types: &mut [Provisional]) {
    let mut edges = Vec::new();
    for (index, contract) in types.iter().enumerate() {
        let ContractKind::Alias { target } = &contract.kind else {
            continue;
        };
        let mut reached = Vec::new();
        target.reaches(false, &mut reached);
        for (to, _) in reached {
            let is_alias = types
                .get(to.index())
                .is_some_and(|it| matches!(it.kind, ContractKind::Alias { .. }));
            if is_alias && let Ok(from) = u32::try_from(index) {
                edges.push((from, to.raw()));
            }
        }
    }
    let cycles = Sccs::of(types.len(), &edges);
    for (index, contract) in types.iter_mut().enumerate() {
        if !cycles.recursive(index) {
            continue;
        }
        if let ContractKind::Alias { target } = &contract.kind {
            contract.kind = ContractKind::Newtype {
                inner: target.clone(),
            };
        }
    }
}

/// Put a `Box` on every reference that closes a cycle without already indirecting.
///
/// The graph that matters is the *type* graph, not the schema graph: merging and deduplication both
/// reshape it, so a decision taken from the schema graph would over-approximate and emit boxes
/// nothing needs. Edges are chosen in a fixed order — by the names involved — so the same document
/// always boxes the same field, which is what makes checked-in output reviewable. rustc's E0072 is
/// the backstop: a miss is loud, never silently wrong.
fn break_cycles(types: &mut [Provisional]) {
    // Each round boxes at least one edge and never adds one, so the number of unboxed direct edges
    // strictly decreases; the bound is the belt-and-braces guard.
    for _ in 0..types.len().saturating_add(1) {
        let mut edges = Vec::new();
        for (index, contract) in types.iter().enumerate() {
            let Ok(from) = u32::try_from(index) else {
                continue;
            };
            for ty in references(&contract.kind) {
                let mut reached = Vec::new();
                ty.reaches(false, &mut reached);
                for (to, indirect) in reached {
                    if !indirect {
                        edges.push((from, to.raw()));
                    }
                }
            }
        }
        let cycles = Sccs::of(types.len(), &edges);
        let Some((from, to)) = smallest_closing_edge(types, &edges, &cycles) else {
            return;
        };
        if let Some(contract) = types.get_mut(from as usize) {
            for ty in references_mut(&mut contract.kind) {
                if box_target(ty, TypeIndex(to)) {
                    break;
                }
            }
        }
    }
}

/// The edge to break, chosen so the choice is reproducible.
///
/// Preference order: an edge out of something other than an alias first (an alias can hold a `Box`,
/// but a struct field reads better with one), then by the two type names.
fn smallest_closing_edge(
    types: &[Provisional],
    edges: &[(u32, u32)],
    cycles: &Sccs,
) -> Option<(u32, u32)> {
    let mut closing: Vec<(bool, String, String, u32, u32)> = edges
        .iter()
        .filter(|&&(from, to)| {
            cycles.together(from as usize, to as usize) && cycles.recursive(from as usize)
        })
        .map(|&(from, to)| {
            let alias = types
                .get(from as usize)
                .is_some_and(|it| matches!(it.kind, ContractKind::Alias { .. }));
            let name_of = |index: u32| {
                types
                    .get(index as usize)
                    .map_or_else(String::new, |it| it.rust_name.to_string())
            };
            (alias, name_of(from), name_of(to), from, to)
        })
        .collect();
    closing.sort();
    closing.first().map(|&(_, _, _, from, to)| (from, to))
}

/// Wrap the first direct reference to `target` in a `Box`.
fn box_target(ty: &mut TypeRef, target: TypeIndex) -> bool {
    match ty {
        TypeRef::Named(index) if *index == target => {
            *ty = TypeRef::Boxed(Box::new(TypeRef::Named(target)));
            true
        }
        TypeRef::Option(inner) | TypeRef::Array(inner, _) => box_target(inner, target),
        TypeRef::Tuple(items) => items.iter_mut().any(|item| box_target(item, target)),
        // Already indirect, so nothing here closes a cycle.
        _ => false,
    }
}

pub(crate) fn references(kind: &ContractKind) -> Vec<&TypeRef> {
    match kind {
        ContractKind::Struct { fields } => fields.iter().map(|field| &field.ty).collect(),
        ContractKind::Enum { variants } => variants.iter().map(|variant| &variant.ty).collect(),
        ContractKind::TaggedEnum { variants, .. }
        | ContractKind::CarriedTagEnum { variants, .. } => {
            variants.iter().map(|variant| &variant.ty).collect()
        }
        ContractKind::StringEnum { .. } => Vec::new(),
        ContractKind::Newtype { inner } => vec![inner],
        ContractKind::Tuple { items } => items.iter().collect(),
        ContractKind::Alias { target } => vec![target],
    }
}

/// Which of the optional derives each type can actually have.
///
/// A fixed point, because support is contagious: a struct can be `Eq` only if every type it holds
/// can be, and that includes types that hold it back. Every round can only remove support, so it
/// settles.
fn supported_derives(types: &[Provisional], config: &Config) -> Vec<BTreeSet<Derive>> {
    let optional = [
        Derive::PartialEq,
        Derive::Eq,
        Derive::Hash,
        Derive::PartialOrd,
        Derive::Ord,
        Derive::Default,
        Derive::Copy,
    ];
    let mut supported: Vec<BTreeSet<Derive>> = types
        .iter()
        .map(|_| optional.iter().copied().collect())
        .collect();

    for _ in 0..=types.len() {
        let mut changed = false;
        for (index, contract) in types.iter().enumerate() {
            let mut still = BTreeSet::new();
            for &derive in &optional {
                if holds(derive, contract, &supported, config) {
                    still.insert(derive);
                }
            }
            if let Some(slot) = supported.get_mut(index)
                && *slot != still
            {
                *slot = still;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    supported
}

/// Whether one type can have one derive, given what its neighbours can have.
fn holds(
    derive: Derive,
    contract: &Provisional,
    supported: &[BTreeSet<Derive>],
    config: &Config,
) -> bool {
    if derive == Derive::Default {
        return defaults(contract, supported, config);
    }
    references(&contract.kind)
        .into_iter()
        .all(|ty| carries(derive, ty, supported, config))
}

/// Whether a type reference is compatible with a derive.
fn carries(derive: Derive, ty: &TypeRef, supported: &[BTreeSet<Derive>], config: &Config) -> bool {
    match ty {
        TypeRef::Named(index) => supported
            .get(index.index())
            .is_some_and(|set| set.contains(&derive)),
        // `serde_json::Value` holds a float and orders nothing.
        TypeRef::Value => derive == Derive::PartialEq || derive == Derive::Default,
        TypeRef::F64 => !matches!(derive, Derive::Eq | Derive::Ord | Derive::Hash),
        // A `String`, and `support::Upload` (a `Vec<u8>` plus options): everything but `Copy`.
        TypeRef::String | TypeRef::Upload => derive != Derive::Copy,
        TypeRef::Format(format) => format_carries(derive, *format, config),
        TypeRef::Vec(inner) => derive != Derive::Copy && carries(derive, inner, supported, config),
        TypeRef::Map(inner) => {
            if derive == Derive::Copy {
                return false;
            }
            // Only `BTreeMap` is ordered and hashable; the other two are the caller's choice and
            // they take these derives away from every type that holds one.
            if matches!(derive, Derive::Ord | Derive::PartialOrd | Derive::Hash)
                && config.map != MapKind::BTreeMap
            {
                return false;
            }
            carries(derive, inner, supported, config)
        }
        TypeRef::Option(inner) | TypeRef::Array(inner, _) => {
            carries(derive, inner, supported, config)
        }
        TypeRef::Boxed(inner) => {
            derive != Derive::Copy && carries(derive, inner, supported, config)
        }
        TypeRef::Tuple(items) => items
            .iter()
            .all(|item| carries(derive, item, supported, config)),
        TypeRef::Unit | TypeRef::Bool | TypeRef::I64 | TypeRef::U64 => true,
    }
}

fn format_carries(derive: Derive, format: Format, config: &Config) -> bool {
    if derive != Derive::Default {
        return derive != Derive::Copy;
    }
    match format {
        Format::DateTime | Format::Date | Format::Time => matches!(
            config.formats.date_time,
            crate::config::DateTimeCrate::String
        ),
        Format::Uuid => matches!(config.formats.uuid, crate::config::UuidCrate::String),
        Format::Base64 | Format::Binary => true,
        Format::Ip | Format::Ipv4 | Format::Ipv6 => false,
    }
}

/// Whether a type can have a `Default`.
///
/// An enum cannot: nothing in the document says which variant is the default one, and picking the
/// first would be progeny inventing a fact.
fn defaults(contract: &Provisional, supported: &[BTreeSet<Derive>], config: &Config) -> bool {
    match &contract.kind {
        ContractKind::Enum { .. }
        | ContractKind::TaggedEnum { .. }
        | ContractKind::CarriedTagEnum { .. }
        | ContractKind::StringEnum { .. } => false,
        ContractKind::Struct { fields } => fields
            .iter()
            .all(|field| carries(Derive::Default, &field.ty, supported, config)),
        ContractKind::Newtype { inner } | ContractKind::Alias { target: inner } => {
            carries(Derive::Default, inner, supported, config)
        }
        ContractKind::Tuple { items } => items
            .iter()
            .all(|item| carries(Derive::Default, item, supported, config)),
    }
}

#[cfg(test)]
mod tests {
    use super::decide;
    use crate::config::{Config, SerdeImpl, UnknownFields};
    use crate::contract::{ContractKind, DeserStrategy, StringVariant, name::RustIdent};

    fn hand_written() -> Config {
        Config {
            serde_impl: SerdeImpl::HandWrittenWhereEligible,
            ..Config::default()
        }
    }

    /// The escape hatch, asked for by name.
    ///
    /// It used to ask `Config::default()` and pass because the default *was* the derive. That made
    /// it a test of the default rather than of the hatch, and it broke the moment the default
    /// became the hand-written path — correctly, but for the wrong reason. A caller reaching for
    /// the hatch names it, so this does too.
    #[test]
    fn the_escape_hatch_derives_everything() {
        let escape = Config {
            serde_impl: SerdeImpl::DeriveAlways,
            ..Config::default()
        };
        for kind in [
            ContractKind::Struct { fields: Vec::new() },
            ContractKind::StringEnum {
                variants: Vec::new(),
            },
        ] {
            assert_eq!(
                decide(&kind, UnknownFields::Ignore, &escape),
                DeserStrategy::Derive,
                "{kind:?} took a hand-written path under the escape hatch"
            );
        }
    }

    /// And the default is the fast one, which is the whole point of stage 8.
    ///
    /// Pinned because it is a product decision rather than an implementation detail: a compile-time
    /// saving nobody gets without setting a flag is not a saving. If this ever flips back it should
    /// be somebody's decision, announced by a failing test, rather than a quiet edit to a `#[default]`.
    #[test]
    fn the_default_is_the_hand_written_path() {
        assert_eq!(
            decide(
                &ContractKind::Struct { fields: Vec::new() },
                UnknownFields::Ignore,
                &Config::default()
            ),
            DeserStrategy::HandWrittenBuffered {
                deny_unknown: false
            }
        );
    }

    #[test]
    fn the_two_paths_measurement_justifies_are_the_two_that_are_taken() {
        let plain = ContractKind::Struct { fields: Vec::new() };
        assert_eq!(
            decide(&plain, UnknownFields::Ignore, &hand_written()),
            DeserStrategy::HandWrittenBuffered {
                deny_unknown: false
            }
        );
        let strings = ContractKind::StringEnum {
            variants: vec![StringVariant {
                rust_name: RustIdent::variant("a"),
                wire_name: "a".to_owned(),
            }],
        };
        assert_eq!(
            decide(&strings, UnknownFields::Ignore, &hand_written()),
            DeserStrategy::HandWrittenFieldless
        );
    }

    #[test]
    fn capturing_unknown_members_is_ruled_to_the_derive() {
        let kind = ContractKind::Struct { fields: Vec::new() };
        assert_eq!(
            decide(&kind, UnknownFields::Capture, &hand_written()),
            DeserStrategy::Derive
        );
    }

    #[test]
    fn a_union_stays_derived_because_converting_it_would_cost_compile_time() {
        for kind in [
            ContractKind::Enum {
                variants: Vec::new(),
            },
            ContractKind::TaggedEnum {
                tag: "kind".to_owned(),
                variants: Vec::new(),
            },
        ] {
            assert_eq!(
                decide(&kind, UnknownFields::Ignore, &hand_written()),
                DeserStrategy::Derive,
                "{kind:?} took a hand-written path"
            );
        }
    }
}
