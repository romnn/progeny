//! Cycles a decoder could go round without consuming any input, and what their types become.
//!
//! A step from one type to another is non-consuming when a decoder hands the next type the same
//! value it was handed.
//! An alias, a newtype, an `Option`, a `Presence` and a `Box` forward it, an untagged union probes
//! each variant with it, a carried-tag union replays the whole object into its variant, and a
//! union flattened beside a struct's members is handed the whole object when no declared member is
//! present.
//! Every other step reads a smaller value: a struct member, a list or tuple element, a map value,
//! a keyed union's payload under its key, and a consumed-tag union's payload without its tag.
//!
//! A cycle of non-consuming steps lets a decoder recurse on one value until the stack overflows,
//! whether derived or hand-written, lenient or strict, so finalization removes every one before
//! anything else runs.
//! Once none is left, every cycle in the type graph consumes input on the way round, and a
//! finite value ends every decode.
//! [`crate::shape::Fit`] judges a `default` or an example that goes round such a cycle by the same
//! rule, so the value it keeps is one the retyped member reads.

use std::collections::BTreeMap;

use super::lower::Provisional;
use super::{Capture, ContractKind, Form, TypeIndex, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};
use crate::schema::cycles::Sccs;

/// Retype every type on a cycle of non-consuming steps, and report each cycle once.
///
/// Each member keeps its name and origin and becomes an alias of what its cycle's [`CycleKind`]
/// says.
/// It runs after twinning, which is sound because both halves of a twin hold the same kind of
/// contract, and every member of a cycle reaches every other, so twinning takes all of a cycle or
/// none of it: a cycle's read twins form a cycle of their own, retyped the same way and reported
/// at the strict half.
pub(super) fn normalize(types: &mut [Provisional], ctx: &mut Ctx) {
    let mut edges = Vec::new();
    for (index, contract) in types.iter().enumerate() {
        let Ok(from) = u32::try_from(index) else {
            continue;
        };
        for ty in replayed(&contract.kind) {
            if let Some(Forwarded { to, .. }) = forwarded(ty) {
                edges.push((from, to.raw()));
            }
        }
    }
    let cycles = Sccs::of(types.len(), &edges);
    let mut members: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for index in 0..types.len() {
        if cycles.recursive(index)
            && let Some(component) = cycles.component(index)
        {
            members.entry(component).or_default().push(index);
        }
    }

    for cycle in members.into_values() {
        let kind = CycleKind::of(types, &cycle);
        for &index in &cycle {
            if let Some(contract) = types.get_mut(index) {
                contract.kind = ContractKind::Alias {
                    target: kind.target(),
                };
            }
        }
        let mut on_cycle: Vec<&Provisional> =
            cycle.iter().filter_map(|&index| types.get(index)).collect();
        // A cycle of read twins mirrors its strict halves' cycle, which is reported already.
        if on_cycle
            .iter()
            .all(|contract| matches!(contract.form, Form::Lenient { strict: Some(_) }))
        {
            continue;
        }
        on_cycle.sort_by(|left, right| left.origin.cmp(&right.origin));
        let names: Vec<String> = on_cycle
            .iter()
            .map(|contract| format!("`{}`", contract.rust_name))
            .collect();
        let Some(detail) = kind.detail(&names) else {
            continue;
        };
        let Some((first, rest)) = on_cycle.split_first() else {
            continue;
        };
        ctx.report(
            Diagnostic::new(kind.class(), kind.action(), first.origin.clone(), detail)
                .with_related(rest.iter().map(|contract| contract.origin.clone())),
        );
    }
}

/// The type references a decoder hands the same value it was handed.
fn replayed(kind: &ContractKind) -> Vec<&TypeRef> {
    match kind {
        ContractKind::Alias { target: inner } | ContractKind::Newtype { inner } => vec![inner],
        ContractKind::Enum { variants, .. } => variants.iter().map(|variant| &variant.ty).collect(),
        ContractKind::CarriedTagEnum { variants, .. } => {
            variants.iter().map(|variant| &variant.ty).collect()
        }
        // A flattened union can be handed the whole object, when no declared member is present.
        ContractKind::Struct { fields } => fields
            .iter()
            .filter(|field| field.capture == Some(Capture::Union))
            .map(|field| &field.ty)
            .collect(),
        // A consumed tag is taken off before the variant reads the rest, a keyed payload sits
        // under its key, a tuple's items are elements, and a string enum names no type.
        ContractKind::TaggedEnum { .. }
        | ContractKind::KeyedEnum { .. }
        | ContractKind::Tuple { .. }
        | ContractKind::StringEnum { .. } => Vec::new(),
    }
}

/// The type a position hands its whole value to, through wrappers alone.
struct Forwarded {
    to: TypeIndex,
    /// Whether an `Option` or a `Presence` on the way reads `null` without forwarding it.
    through_null: bool,
}

/// The type `ty` hands its whole value to, if it hands it to one.
fn forwarded(ty: &TypeRef) -> Option<Forwarded> {
    match ty {
        TypeRef::Named(to) => Some(Forwarded {
            to: *to,
            through_null: false,
        }),
        TypeRef::Boxed(inner) => forwarded(inner),
        TypeRef::Option(inner) | TypeRef::Presence(inner) => {
            forwarded(inner).map(|found| Forwarded {
                through_null: true,
                ..found
            })
        }
        _ => None,
    }
}

/// What a cycle of non-consuming steps leaves a decoder to stop at, and so what it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleKind {
    /// Aliases and boxes alone, typed as `serde_json::Value`.
    ///
    /// Nothing on the cycle is a member, an element or a `null`, so it describes no finite value;
    /// two single-branch `oneOf`s naming each other are the usual spelling.
    Nonproductive,
    /// Wrappers alone, some of them optional, typed as `()`.
    ///
    /// `null` is the one finite value, as in `A(Option<Box<A>>)`, and `()` reads and writes
    /// exactly that.
    NullOnly,
    /// Through a union, typed as `serde_json::Value`.
    ///
    /// The cycle's other alternatives and the union's open arm are finite values, and arbitrary
    /// JSON keeps them all.
    /// Keeping the cycle's members typed while dropping only its recursive steps would need each
    /// member to inherit the alternatives and branch order of every member it reaches, which
    /// means propagating exits through the strongly connected component; this pass deliberately
    /// does not, and types the whole cycle as arbitrary JSON instead.
    Replaying,
}

impl CycleKind {
    fn of(types: &[Provisional], cycle: &[usize]) -> Self {
        let mut through_null = false;
        for contract in cycle.iter().filter_map(|&index| types.get(index)) {
            // A wrapper on a cycle forwards to the next member, its only step.
            let (ContractKind::Alias { target: inner } | ContractKind::Newtype { inner }) =
                &contract.kind
            else {
                return Self::Replaying;
            };
            through_null |= forwarded(inner).is_some_and(|found| found.through_null);
        }
        if through_null {
            Self::NullOnly
        } else {
            Self::Nonproductive
        }
    }

    fn target(self) -> TypeRef {
        match self {
            Self::Nonproductive | Self::Replaying => TypeRef::Value,
            Self::NullOnly => TypeRef::Unit,
        }
    }

    fn class(self) -> BreakageClass {
        match self {
            Self::Nonproductive => BreakageClass::NonproductiveCycle,
            Self::NullOnly | Self::Replaying => BreakageClass::NonConsumingCycle,
        }
    }

    /// `()` holds exactly the values the cycle describes, so typing it so loses nothing.
    fn action(self) -> Action {
        match self {
            Self::Nonproductive | Self::Replaying => Action::Degrade,
            Self::NullOnly => Action::Repair,
        }
    }

    /// The record's sentence, about the members `names` spells in order.
    fn detail(self, names: &[String]) -> Option<String> {
        let detail = match (self, names) {
            (_, []) => return None,
            (Self::Nonproductive, [only]) => format!(
                "{only} lowers to aliases and wrappers of itself that never reach a member, an \
                 element or a `null`, so no finite value is one; typed it as `serde_json::Value`"
            ),
            (Self::Nonproductive, [init @ .., last]) => format!(
                "{} and {last} lower to aliases and wrappers of one another that never reach a \
                 member, an element or a `null`, so no finite value is any of them; typed each \
                 as `serde_json::Value`",
                init.join(", ")
            ),
            (Self::NullOnly, [only]) => format!(
                "{only} lowers to wrappers of itself whose only way out is `null`, so a decoder \
                 handed any other value would read it again forever; typed it as `()`, which \
                 reads and writes `null`, its one finite value"
            ),
            (Self::NullOnly, [init @ .., last]) => format!(
                "{} and {last} lower to wrappers of one another whose only way out is `null`, so \
                 a decoder handed any other value would read it again forever; typed each as \
                 `()`, which reads and writes `null`, their one finite value",
                init.join(", ")
            ),
            (Self::Replaying, [only]) => format!(
                "{only} reaches itself through unions and wrappers that hand a decoder the same \
                 value again, so one its other alternatives do not take would be read forever; \
                 typed it as `serde_json::Value`"
            ),
            (Self::Replaying, [init @ .., last]) => format!(
                "{} and {last} reach one another through unions and wrappers that hand a decoder \
                 the same value again, so one their other alternatives do not take would be read \
                 forever; typed each as `serde_json::Value`",
                init.join(", ")
            ),
        };
        Some(detail)
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::{Value, json};

    use super::normalize;
    use crate::config::{Config, UnknownFields};
    use crate::contract::lower::Provisional;
    use crate::contract::tests::{contracts_of, named, with_schemas};
    use crate::contract::{
        Capture, ContractKind, FieldContract, Form, KeyedVariant, Presence, SkipRule, TagRoutes,
        TaggedVariant, TypeIndex, TypeRef, VariantContract, name::RustIdent,
    };
    use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
    use crate::shape::Docs;

    /// The contracts of `schemas`, and the cycles of either class reported about them.
    fn cycles_of(schemas: Value) -> eyre::Result<(crate::contract::Contracts, Vec<Diagnostic>)> {
        let (contracts, diagnostics) = contracts_of(with_schemas(schemas), &Config::default())?;
        let cycles = diagnostics
            .into_iter()
            .filter(|found| {
                matches!(
                    found.class(),
                    BreakageClass::NonproductiveCycle | BreakageClass::NonConsumingCycle
                )
            })
            .collect();
        Ok((contracts, cycles))
    }

    /// Two single-branch unions naming each other describe no finite value, so both become
    /// arbitrary JSON, and the cycle is reported once, at its first member, naming the other.
    #[test_util::test]
    fn two_unions_naming_only_each_other_are_typed_as_arbitrary_json() {
        let (contracts, cycles) = cycles_of(json!({
            "A": {"oneOf": [{"$ref": "#/components/schemas/B"}]},
            "B": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
        }))?;
        for name in ["A", "B"] {
            assert_eq!(
                named(&contracts, name)?.kind(),
                &ContractKind::Alias {
                    target: TypeRef::Value
                },
                "{name}"
            );
        }
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.class(), BreakageClass::NonproductiveCycle);
        assert_eq!(cycle.action(), Action::Degrade);
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        let related: Vec<String> = cycle.related().iter().map(ToString::to_string).collect();
        assert_eq!(related, ["/components/schemas/B"]);
        assert!(cycle.detail().contains("`A` and `B`"), "{}", cycle.detail());
    }

    /// Two separate cycles are two records, and an alias that only leads into a cycle keeps its
    /// own contract rather than joining it.
    #[test_util::test]
    fn separate_cycles_are_reported_apart_and_an_alias_into_one_is_not_part_of_it() {
        let (contracts, cycles) = cycles_of(json!({
            "A": {"oneOf": [{"$ref": "#/components/schemas/B"}]},
            "B": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
            "C": {"oneOf": [{"$ref": "#/components/schemas/C"}]},
            "Into": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
        }))?;
        // Both cycles degrade, one record each
        for name in ["A", "B", "C"] {
            assert_eq!(
                named(&contracts, name)?.kind(),
                &ContractKind::Alias {
                    target: TypeRef::Value
                },
                "{name}"
            );
        }
        let mut locations: Vec<String> = cycles
            .iter()
            .map(|cycle| cycle.location().to_string())
            .collect();
        locations.sort();
        assert_eq!(
            locations,
            ["/components/schemas/A", "/components/schemas/C"],
            "{cycles:#?}"
        );
        // The alias into the cycle is reported nowhere and still names its target
        assert!(
            cycles.iter().all(|cycle| !cycle
                .related()
                .iter()
                .any(|related| related.to_string().ends_with("/Into"))),
            "{cycles:#?}"
        );
        assert_ne!(
            named(&contracts, "Into")?.kind(),
            &ContractKind::Alias {
                target: TypeRef::Value
            }
        );
    }

    /// The shortest nonproductive cycle: a union whose one branch is itself.
    #[test_util::test]
    fn a_union_naming_only_itself_is_typed_as_arbitrary_json() {
        let (contracts, cycles) = cycles_of(json!({
            "A": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
        }))?;
        assert_eq!(
            named(&contracts, "A")?.kind(),
            &ContractKind::Alias {
                target: TypeRef::Value
            }
        );
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        assert!(cycle.related().is_empty(), "{cycle:#?}");
    }

    /// An empty list, an empty map and an absent member are finite values reached by consuming
    /// input, so a cycle through any of them is an ordinary recursive type: kept as written, and
    /// not reported.
    #[test_util::test]
    fn a_cycle_through_a_list_a_map_or_a_member_is_kept() {
        let (contracts, cycles) = cycles_of(json!({
            "Tree": {"type": "array", "items": {"$ref": "#/components/schemas/Tree"}},
            "Index": {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/Index"}},
            "Node": {"type": "object", "properties": {"next": {"$ref": "#/components/schemas/Node"}}},
        }))?;
        assert!(cycles.is_empty(), "{cycles:#?}");
        for name in ["Tree", "Index"] {
            assert!(
                matches!(
                    named(&contracts, name)?.kind(),
                    ContractKind::Newtype { .. }
                ),
                "{name} is {:?}",
                named(&contracts, name)?.kind()
            );
        }
        assert!(matches!(
            named(&contracts, "Node")?.kind(),
            ContractKind::Struct { .. }
        ));
    }

    /// A wrapper around itself whose only way out is `null` has `null` as its one finite value,
    /// so it becomes `()`, which refuses every other value instead of reading it again forever.
    /// The struct holding it keeps its member.
    #[test_util::test]
    fn a_wrapper_whose_only_way_out_is_null_is_typed_as_null() {
        let (contracts, cycles) = cycles_of(json!({
            "Maybe": {"oneOf": [{"$ref": "#/components/schemas/Maybe"}, {"type": "null"}]},
            "Holder": {"type": "object", "required": ["a"], "properties": {
                "a": {"$ref": "#/components/schemas/Maybe"},
            }},
        }))?;
        assert_eq!(
            named(&contracts, "Maybe")?.kind(),
            &ContractKind::Alias {
                target: TypeRef::Unit
            }
        );
        assert!(matches!(
            named(&contracts, "Holder")?.kind(),
            ContractKind::Struct { fields } if fields.len() == 1
        ));
        // Reported as a repair: `()` holds exactly the values the schema describes
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.class(), BreakageClass::NonConsumingCycle);
        assert_eq!(cycle.action(), Action::Repair);
        assert_eq!(cycle.location().to_string(), "/components/schemas/Maybe");
        assert!(cycle.related().is_empty(), "{cycle:#?}");
        assert!(
            cycle.detail().contains("typed it as `()`"),
            "{}",
            cycle.detail()
        );
    }

    /// Two wrappers of one another with a `null` between them are one cycle, typed and reported
    /// together.
    #[test_util::test]
    fn wrappers_of_one_another_whose_only_way_out_is_null_are_typed_as_null_together() {
        let (contracts, cycles) = cycles_of(json!({
            "A": {"oneOf": [{"$ref": "#/components/schemas/B"}, {"type": "null"}]},
            "B": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
        }))?;
        for name in ["A", "B"] {
            assert_eq!(
                named(&contracts, name)?.kind(),
                &ContractKind::Alias {
                    target: TypeRef::Unit
                },
                "{name}"
            );
        }
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.class(), BreakageClass::NonConsumingCycle);
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        let related: Vec<String> = cycle.related().iter().map(ToString::to_string).collect();
        assert_eq!(related, ["/components/schemas/B"]);
        assert!(cycle.detail().contains("`A` and `B`"), "{}", cycle.detail());
    }

    /// A required member typed through an alias by a wrapper that holds `null` is nullable, as
    /// one typed by the wrapper itself is, and keeps an `Option` to read it in when the cycle it
    /// names becomes `()`.
    #[test_util::test]
    fn a_required_member_behind_an_alias_of_a_null_only_cycle_reads_null_in_an_option() {
        let (contracts, _) = cycles_of(json!({
            "Maybe": {"oneOf": [{"$ref": "#/components/schemas/Maybe"}, {"type": "null"}]},
            "Via": {"oneOf": [{"$ref": "#/components/schemas/Maybe"}]},
            "Loop": {"oneOf": [{"$ref": "#/components/schemas/LoopOpt"}]},
            "LoopOpt": {"oneOf": [{"$ref": "#/components/schemas/Loop"}, {"type": "null"}]},
            "Holder": {"type": "object", "required": ["via", "loop"], "properties": {
                "via": {"$ref": "#/components/schemas/Via"},
                "loop": {"$ref": "#/components/schemas/Loop"},
            }},
        }))?;
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            eyre::bail!("`Holder` is a struct");
        };
        // Both members say they may be `null`, so a read form keeps the `null` they hold
        for field in fields {
            assert_eq!(field.presence, Presence::Nullable, "{}", field.wire_name);
        }
        // `Via` names `Option<Maybe>`; `Loop` became `()`, so its member wraps it again
        let loop_member = fields
            .iter()
            .find(|field| field.wire_name == "loop")
            .ok_or_else(|| eyre::eyre!("the `loop` member"))?;
        assert!(
            matches!(&loop_member.ty, TypeRef::Option(inner) if matches!(**inner, TypeRef::Named(_))),
            "{:?}",
            loop_member.ty
        );
    }

    /// A `default` on a member typed by a null-only cycle is judged as the `()` it became: a
    /// `null` is kept, and anything else is dropped as a default that contradicts its schema.
    #[test_util::test]
    fn a_default_on_a_member_typed_by_a_null_only_cycle_must_be_null() {
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Holder": {"type": "object", "required": ["kept"], "properties": {
                    "kept": {"$ref": "#/components/schemas/Maybe", "default": null},
                    "dropped": {"$ref": "#/components/schemas/Maybe", "default": 1},
                }},
                "Maybe": {"oneOf": [{"$ref": "#/components/schemas/Maybe"}, {"type": "null"}]},
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            eyre::bail!("`Holder` is a struct");
        };
        let defaults: Vec<(&str, Option<&Value>)> = fields
            .iter()
            .map(|field| (field.wire_name.as_str(), field.default.as_ref()))
            .collect();
        assert_eq!(
            defaults,
            [("dropped", None), ("kept", Some(&Value::Null))],
            "{defaults:?}"
        );
        let invalid: Vec<String> = diagnostics
            .iter()
            .filter(|found| found.class() == BreakageClass::InvalidDefault)
            .map(|found| found.location().to_string())
            .collect();
        assert_eq!(
            invalid,
            ["/components/schemas/Holder/properties/dropped/default"]
        );
    }

    /// A contract named `name`, declared as that component, in its strict form.
    fn declared(name: &str, kind: ContractKind) -> Provisional {
        Provisional {
            rust_name: RustIdent::type_name(&[name.to_owned()]),
            docs: Docs::default(),
            kind,
            unknown_fields: UnknownFields::Ignore,
            origin: JsonPointer::root()
                .child("components")
                .child("schemas")
                .child(name),
            declared: true,
            explicit: false,
            component: Some(name.to_owned()),
            form: Form::Strict { twin: None },
        }
    }

    /// The type at `index`, boxed as a cycle-closing reference is.
    fn boxed(index: u32) -> TypeRef {
        TypeRef::Boxed(Box::new(TypeRef::Named(TypeIndex(index))))
    }

    /// An untagged union of `types`, with its open arm.
    fn untagged(types: Vec<TypeRef>) -> ContractKind {
        ContractKind::Enum {
            variants: types
                .into_iter()
                .enumerate()
                .map(|(index, ty)| VariantContract {
                    rust_name: RustIdent::variant(&format!("V{index}")),
                    ty,
                })
                .collect(),
            fallback: RustIdent::fallback_variant(),
        }
    }

    /// A union read by a `kind` tag, with one variant holding `ty` under the tag `self`.
    fn tagged(ty: TypeRef) -> Vec<TaggedVariant> {
        vec![TaggedVariant {
            rust_name: RustIdent::variant("Itself"),
            ty,
            tag_value: "self".to_owned(),
            aliases: Vec::new(),
        }]
    }

    /// A member called `name`, typed `ty`, flattened as `capture` says.
    fn member(name: &str, ty: TypeRef, capture: Option<Capture>) -> FieldContract {
        FieldContract {
            rust_name: RustIdent::field(name),
            wire_name: name.to_owned(),
            ty,
            presence: Presence::Optional,
            default: None,
            skip_serializing_if: SkipRule::WhenNone,
            capture,
            docs: Docs::default(),
        }
    }

    /// The contracts `normalize` leaves, and what it reported.
    fn normalized(mut types: Vec<Provisional>) -> (Vec<Provisional>, Vec<Diagnostic>) {
        let mut ctx = Ctx::new();
        normalize(&mut types, &mut ctx);
        (types, ctx.into_diagnostics())
    }

    /// Whether every contract became an alias of `target`.
    fn all_aliases_of(types: &[Provisional], target: &TypeRef) -> bool {
        types.iter().all(
            |contract| matches!(&contract.kind, ContractKind::Alias { target: it } if it == target),
        )
    }

    /// An untagged union probes its variants with the value it was handed, so one listing itself
    /// would read a value none of its other variants takes forever.
    /// It becomes arbitrary JSON, which keeps the other alternatives and the open arm.
    #[test_util::test]
    fn an_untagged_union_listing_itself_is_typed_as_arbitrary_json() {
        let (types, cycles) = normalized(vec![declared(
            "A",
            untagged(vec![boxed(0), TypeRef::String]),
        )]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.class(), BreakageClass::NonConsumingCycle);
        assert_eq!(cycle.action(), Action::Degrade);
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        assert!(cycle.related().is_empty(), "{cycle:#?}");
    }

    /// Two untagged unions listing each other are one cycle: both become arbitrary JSON, and one
    /// record at the first names the second.
    #[test_util::test]
    fn untagged_unions_listing_each_other_are_one_cycle() {
        let (types, cycles) = normalized(vec![
            declared("A", untagged(vec![boxed(1), TypeRef::String])),
            declared(
                "B",
                untagged(vec![boxed(0), TypeRef::Integer(crate::shape::IntType::I64)]),
            ),
        ]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.class(), BreakageClass::NonConsumingCycle);
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        let related: Vec<String> = cycle.related().iter().map(ToString::to_string).collect();
        assert_eq!(related, ["/components/schemas/B"]);
        assert!(cycle.detail().contains("`A` and `B`"), "{}", cycle.detail());
    }

    /// A carried-tag union replays the whole object into its variant, and a union flattened
    /// beside a struct's members is handed the whole object when none of them is present, so a
    /// cycle through either reads one value forever.
    #[test_util::test]
    fn a_cycle_through_a_carried_tag_or_a_flattened_union_is_typed_as_arbitrary_json() {
        // A carried-tag union whose one variant is itself
        let (types, cycles) = normalized(vec![declared(
            "Carried",
            ContractKind::CarriedTagEnum {
                tag: "kind".to_owned(),
                variants: tagged(boxed(0)),
                routes: TagRoutes::default(),
                fallback: RustIdent::fallback_variant(),
            },
        )]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        assert_eq!(cycles.len(), 1, "{cycles:#?}");

        // A struct whose flattened union lists the struct
        let (types, cycles) = normalized(vec![
            declared(
                "Holder",
                ContractKind::Struct {
                    fields: vec![
                        member("label", TypeRef::Option(Box::new(TypeRef::String)), None),
                        member("variant", boxed(1), Some(Capture::Union)),
                    ],
                },
            ),
            declared("Either", untagged(vec![boxed(0)])),
        ]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        assert_eq!(cycles.len(), 1, "{cycles:#?}");
    }

    /// Optional wrappers on a cycle that also passes through a union do not make it null-only:
    /// the union's other alternatives are values too, so the cycle becomes arbitrary JSON.
    #[test_util::test]
    fn wrappers_on_a_cycle_through_a_union_are_typed_as_arbitrary_json() {
        let (types, cycles) = normalized(vec![
            declared(
                "Maybe",
                ContractKind::Alias {
                    target: TypeRef::Option(Box::new(TypeRef::Named(TypeIndex(1)))),
                },
            ),
            declared("Either", untagged(vec![boxed(0), TypeRef::String])),
        ]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.action(), Action::Degrade);
    }

    /// A consumed tag, a keyed payload, a struct member, a list or map element and a tuple item
    /// each read a smaller value than the one handed over, so a cycle through any of them ends
    /// on finite input and keeps its types.
    #[test_util::test]
    fn a_cycle_through_a_step_that_consumes_input_is_kept() {
        let types = vec![
            declared(
                "Tagged",
                ContractKind::TaggedEnum {
                    tag: "kind".to_owned(),
                    variants: tagged(boxed(0)),
                    fallback: RustIdent::fallback_variant(),
                },
            ),
            declared(
                "Keyed",
                ContractKind::KeyedEnum {
                    variants: vec![KeyedVariant {
                        rust_name: RustIdent::variant("Itself"),
                        wire_name: "self".to_owned(),
                        payload: Some(boxed(1)),
                    }],
                    fallback: RustIdent::fallback_variant(),
                },
            ),
            declared(
                "Linked",
                ContractKind::Struct {
                    fields: vec![member("next", TypeRef::Option(Box::new(boxed(2))), None)],
                },
            ),
            declared(
                "List",
                ContractKind::Newtype {
                    inner: TypeRef::Vec(Box::new(TypeRef::Named(TypeIndex(3)))),
                },
            ),
            declared(
                "Index",
                ContractKind::Newtype {
                    inner: TypeRef::Map(Box::new(TypeRef::Named(TypeIndex(4)))),
                },
            ),
            declared(
                "Pair",
                ContractKind::Tuple {
                    items: vec![boxed(5), TypeRef::String],
                },
            ),
            // An untagged union whose variant reaches it again only through a list element
            declared(
                "Nested",
                untagged(vec![TypeRef::Vec(Box::new(TypeRef::Named(TypeIndex(6))))]),
            ),
        ];
        let (normalized, cycles) = normalized(types.clone());
        assert!(cycles.is_empty(), "{cycles:#?}");
        let kinds = |types: &[Provisional]| -> Vec<ContractKind> {
            types.iter().map(|contract| contract.kind.clone()).collect()
        };
        assert_eq!(kinds(&normalized), kinds(&types));
    }

    /// A cycle and its read twins are two cycles of one shape: both are retyped alike, so the
    /// conversion between the halves stays an identity, and the pair is reported once, at the
    /// strict half.
    #[test_util::test]
    fn a_cycle_and_its_read_twins_are_retyped_alike_and_reported_once() {
        let mut strict = declared("A", untagged(vec![boxed(0), TypeRef::String]));
        strict.form = Form::Strict {
            twin: Some(TypeIndex(1)),
        };
        let mut read = declared("A", untagged(vec![boxed(1), TypeRef::String]));
        read.form = Form::Lenient {
            strict: Some(TypeIndex(0)),
        };
        let (types, cycles) = normalized(vec![strict, read]);
        assert!(all_aliases_of(&types, &TypeRef::Value), "{types:#?}");
        let [cycle] = cycles.as_slice() else {
            eyre::bail!("one cycle is one record: {cycles:#?}");
        };
        assert_eq!(cycle.location().to_string(), "/components/schemas/A");
        assert!(cycle.related().is_empty(), "{cycle:#?}");
    }

    /// Judging a `default` walks the shapes before any contract exists, so it meets the cycle
    /// itself; it has to end, and a cycle that constrains nothing finds nothing wrong.
    #[test_util::test]
    fn a_default_on_a_member_typed_by_a_nonproductive_cycle_is_judged() {
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Holder": {"type": "object", "properties": {
                    "a": {"$ref": "#/components/schemas/A", "default": 1},
                }},
                "A": {"oneOf": [{"$ref": "#/components/schemas/B"}]},
                "B": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
            })),
            &Config::default(),
        )?;
        assert!(
            !diagnostics
                .iter()
                .any(|found| found.class() == BreakageClass::InvalidDefault),
            "{diagnostics:#?}"
        );
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            eyre::bail!("`Holder` is a struct");
        };
        let defaults: Vec<Option<&Value>> =
            fields.iter().map(|field| field.default.as_ref()).collect();
        assert_eq!(defaults, [Some(&json!(1))]);
    }
}
