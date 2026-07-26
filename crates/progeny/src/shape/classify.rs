//! The classification itself: one merged view of a key becomes one shape.
//!
//! Every arm here is a shape real documents produce. What is *not* here is a general fallback:
//! constructs outside the set become [`Shape::Any`] with a diagnostic, which is the designed
//! behaviour rather than a gap. Two rules keep that honest:
//!
//! * **Degrade outward, never inward.** A keyword progeny does not turn into a type — `not`,
//!   `if`/`then`, `contains` — makes the generated type accept a *superset* of what the document
//!   allows. Accepting a payload the vendor would reject is a tolerance decision; rejecting one
//!   they would accept would be a bug in the generated client.
//! * **Nothing degrades quietly.** Every arm that gives up says so through `ctx`.

use std::collections::BTreeSet;

use serde_json::Value;

use super::merge::{self, View};
use super::{
    Docs, Extra, Field, Format, Scalar, Shape, ShapeKey, ShapeRef, Struct, Union, Variant,
};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::TypeName;

/// The largest fixed-length array progeny emits.
///
/// `[T; N]` is a distinct type per length, and serde's own array impls stop being free above the
/// small sizes; past this a longer `minItems == maxItems` array is a `Vec`, which accepts a
/// superset — the sound direction.
const MAX_FIXED_ARRAY: u32 = 32;

/// Classify one key.
pub(crate) fn key(resolved: &ResolvedDocument, key: &ShapeKey, ctx: &mut Ctx) -> Shape {
    let mut view = merge::view(resolved, key);
    let at = key.anchor().map_or_else(JsonPointer::root, |id| {
        resolved.schemas().address(id).clone()
    });

    if view.impossible {
        degrade(
            ctx,
            &at,
            BreakageClass::UnsupportedConstruct,
            "a schema in this position accepts no value at all, which no Rust type expresses",
        );
        return Shape::Any;
    }
    if let Some(conflict) = view.conflict.take() {
        degrade(ctx, &at, BreakageClass::IrreconcilableAllOf, &conflict);
        return Shape::Any;
    }
    if view.unions_collide {
        degrade(
            ctx,
            &at,
            BreakageClass::IrreconcilableAllOf,
            "two branches of this schema each declare their own `oneOf`/`anyOf`, and the \
             intersection of two unions has no faithful Rust type",
        );
        return Shape::Any;
    }
    report_unknown_types(&mut view, ctx, &at);
    report_uninterpreted(&view, ctx, &at);

    nullable(resolved, &view, ctx, &at)
}

/// Peel `null` off the front, since every other arm is about a value that is not null.
fn nullable(resolved: &ResolvedDocument, view: &View, ctx: &mut Ctx, at: &JsonPointer) -> Shape {
    let nulls = view
        .types
        .as_ref()
        .is_some_and(|types| types.contains(&TypeName::Null));
    let others: Option<BTreeSet<TypeName>> = view.types.as_ref().map(|types| {
        types
            .iter()
            .filter(|name| **name != TypeName::Null)
            .cloned()
            .collect()
    });
    if nulls && others.as_ref().is_some_and(BTreeSet::is_empty) {
        return Shape::Null;
    }

    let core = core(resolved, view, others.as_ref(), ctx, at);
    // An `enum` widened with `null` is nullable too, which is how 3.0's `nullable: true` beside an
    // enum arrives here after normalization.
    let enum_null = view
        .enumeration
        .as_ref()
        .is_some_and(|values| values.iter().any(Value::is_null));
    if nulls || enum_null {
        return Shape::Optional(ShapeRef::Inline(Box::new(core)));
    }
    core
}

/// Everything but nullability.
fn core(
    resolved: &ResolvedDocument,
    view: &View,
    types: Option<&BTreeSet<TypeName>>,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    if let Some(branches) = &view.union {
        return union(resolved, view, branches, ctx, at);
    }
    if let Some(values) = string_values(view, ctx, at) {
        return Shape::StringEnum(values);
    }

    let mut named = types.into_iter().flatten();
    let (first, second) = (named.next(), named.next());
    if second.is_some() {
        degrade(
            ctx,
            at,
            BreakageClass::UnsupportedConstruct,
            "`type` names more than one kind of value, which no single Rust type expresses",
        );
        return Shape::Any;
    }

    match first {
        Some(TypeName::Object) => object(view),
        Some(TypeName::Array) => array(view),
        Some(TypeName::String) => string(view),
        Some(TypeName::Integer) => Shape::Scalar(Scalar::Integer {
            signed: !view.unsigned,
        }),
        Some(TypeName::Number) => Shape::Scalar(Scalar::Number),
        Some(TypeName::Boolean) => Shape::Scalar(Scalar::Bool),
        Some(TypeName::Null) => Shape::Null,
        // `type` names something progeny does not know; the diagnostic is already recorded.
        Some(TypeName::Other(_)) => Shape::Any,
        // A document that omits `type` but says what the members are means an object, and the
        // corpus is full of both spellings. Guessing from the other keywords is not a repair: it
        // is what the schema says.
        None => untyped(view),
    }
}

fn untyped(view: &View) -> Shape {
    if !view.properties.is_empty() || view.additional.is_some() || view.additional_denied {
        return object(view);
    }
    if view.items.is_some() || view.prefix_items.is_some() {
        return array(view);
    }
    if view.uniform_pattern.is_some() {
        return object(view);
    }
    if view.format.is_some() || view.content_encoding.is_some() || view.content_media_type.is_some()
    {
        return string(view);
    }
    Shape::Any
}

fn object(view: &View) -> Shape {
    if view.properties.is_empty() && !view.additional_denied {
        // No declared names: the value is a map, whatever its values are.
        if let Some(value) = view
            .additional
            .clone()
            .or_else(|| view.uniform_pattern.clone())
        {
            return Shape::Map {
                value: Some(ShapeRef::Key(value)),
            };
        }
        return Shape::Map { value: None };
    }

    let fields = view
        .properties
        .iter()
        .map(|(name, key)| Field {
            wire: name.clone(),
            shape: ShapeRef::Key(key.clone()),
            required: view.required.contains(name),
            docs: view.property_docs.get(name).cloned().unwrap_or_default(),
            default: view.property_defaults.get(name).cloned(),
            read_only: view.read_only.contains(name),
            write_only: view.write_only.contains(name),
        })
        .collect();
    let extra = if view.additional_denied {
        Extra::Denied
    } else if let Some(value) = view
        .additional
        .clone()
        .or_else(|| view.uniform_pattern.clone())
    {
        Extra::Typed(ShapeRef::Key(value))
    } else {
        Extra::Open
    };
    Shape::Struct(Struct { fields, extra })
}

fn array(view: &View) -> Shape {
    let item = view.items.clone().map(ShapeRef::Key);
    if let Some(prefix) = &view.prefix_items {
        // An empty `prefixItems` constrains no position, so it is an array and not a tuple of
        // nothing — which would be the unit type and would serialize as `null`.
        if !prefix.is_empty() {
            return Shape::Tuple {
                items: prefix.iter().cloned().map(ShapeRef::Key).collect(),
                rest: item,
            };
        }
    }
    if let (Some(min), Some(max), Some(item)) = (view.min_items, view.max_items, item.clone())
        && min == max
        && min > 0
        && min <= MAX_FIXED_ARRAY
    {
        return Shape::FixedArray { item, len: min };
    }
    Shape::Array { item }
}

fn string(view: &View) -> Shape {
    if view.content_encoding.as_deref() == Some("base64") {
        return Shape::Format(Format::Base64);
    }
    if view.content_media_type.as_deref() == Some("application/octet-stream") {
        return Shape::Format(Format::Binary);
    }
    match view.format.as_deref() {
        Some("date-time") => Shape::Format(Format::DateTime),
        Some("date") => Shape::Format(Format::Date),
        Some("time" | "partial-time") => Shape::Format(Format::Time),
        Some("uuid") => Shape::Format(Format::Uuid),
        // Every other `format` is an annotation progeny keeps and gives no type of its own.
        _ => Shape::Scalar(Scalar::String),
    }
}

/// The union table, minus the discriminator's tag dispatch.
///
/// Four rows, in the order the corpus makes them worth having: nullable emulation (83% of every
/// `anyOf` in the corpus), an enumeration of constants, one branch, and everything else as an
/// untagged enum. The fifth row is the degradation: a declared discriminator whose variants cannot
/// be told apart structurally.
fn union(
    resolved: &ResolvedDocument,
    view: &View,
    branches: &[ShapeKey],
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let mut nulls = 0;
    let mut rest: Vec<ShapeKey> = Vec::new();
    for branch in branches {
        if is_null(resolved, branch) {
            nulls += 1;
        } else if !rest.contains(branch) {
            rest.push(branch.clone());
        }
    }

    let inner = match rest.len() {
        0 => {
            return if nulls > 0 { Shape::Null } else { Shape::Any };
        }
        // `anyOf: [T, {type: null}]` — the nullable-emulation pattern, and the reason to look at
        // the branches at all before treating this as a union.
        1 => rest
            .into_iter()
            .next()
            .map_or(Shape::Any, |key| Shape::Alias(ShapeRef::Key(key))),
        _ => {
            if let Some(values) = branch_constants(resolved, &rest) {
                Shape::StringEnum(values)
            } else if let Some(property) = &view.discriminator
                && !distinguishable(resolved, &rest)
            {
                // A discriminator says "read the tag to know the variant". Consuming it as a
                // serde tag is stage 4's work; until then variants are matched structurally,
                // which is faithful only when they can be told apart that way. When they cannot,
                // an untagged enum would pick whichever variant happens to deserialize first —
                // silently wrong output, which is the one forbidden failure mode.
                degrade(
                    ctx,
                    at,
                    BreakageClass::DiscriminatorEdgeCase,
                    &format!(
                        "this union is told apart by the `{property}` property, and its variants \
                         cannot be distinguished by their required properties alone"
                    ),
                );
                Shape::Any
            } else {
                Shape::Union(Union {
                    variants: rest
                        .into_iter()
                        .map(|key| Variant {
                            shape: ShapeRef::Key(key),
                        })
                        .collect(),
                })
            }
        }
    };

    if nulls > 0 {
        return Shape::Optional(ShapeRef::Inline(Box::new(inner)));
    }
    inner
}

/// Whether a branch is the `null` type and nothing else.
fn is_null(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    let view = merge::view(resolved, key);
    view.types
        .as_ref()
        .is_some_and(|types| types.len() == 1 && types.contains(&TypeName::Null))
}

/// The strings an `enum`/`const` names, when every value is one.
fn string_values(view: &View, ctx: &mut Ctx, at: &JsonPointer) -> Option<Vec<String>> {
    let declared = match (&view.enumeration, &view.constant) {
        (Some(values), _) => values.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    // `null` is nullability rather than a value, and the caller has already read it as such.
    let values: Vec<&Value> = declared.iter().filter(|value| !value.is_null()).collect();
    if values.is_empty() {
        return None;
    }
    if values.iter().all(|value| value.is_string()) {
        let mut names: Vec<String> = values
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect();
        names.dedup();
        return Some(names);
    }
    // Values of one non-string kind — a numeric or boolean enumeration — fall back to that kind's
    // scalar, which still accepts every listed value; only the narrowing is lost, and there is no
    // Rust form for it to be lost from. Values of *mixed* kinds have no common type at all, which
    // is the case 02 calls out and the one worth reporting.
    if !values.iter().all(|value| same_kind(value, values.first())) {
        degrade(
            ctx,
            at,
            BreakageClass::UnsupportedConstruct,
            "`enum` mixes values of different kinds, so no single Rust type holds them; the value \
             is typed as arbitrary JSON",
        );
    }
    None
}

/// Whether two JSON values are the same kind of thing, for the mixed-enum test.
fn same_kind(value: &Value, against: Option<&&Value>) -> bool {
    let Some(against) = against else {
        return true;
    };
    std::mem::discriminant(value) == std::mem::discriminant(*against)
}

/// The strings a union's branches name, when every branch is a single constant string.
fn branch_constants(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> Option<Vec<String>> {
    let mut values = Vec::new();
    for branch in branches {
        let view = merge::view(resolved, branch);
        let declared = match (&view.enumeration, &view.constant) {
            (Some(listed), _) if listed.len() == 1 => listed.first().cloned(),
            (None, Some(value)) => Some(value.clone()),
            _ => None,
        }?;
        values.push(declared.as_str()?.to_owned());
    }
    (!values.is_empty()).then_some(values)
}

/// Whether every pair of branches has a required property the other does not declare at all.
///
/// The test serde's untagged matching actually needs: a payload for one branch must fail to
/// deserialize as any other. Required-and-absent is the conservative form of that, and being
/// conservative here means degrading rather than matching the wrong branch.
fn distinguishable(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> bool {
    let views: Vec<(BTreeSet<TypeName>, BTreeSet<String>, BTreeSet<String>)> = branches
        .iter()
        .map(|branch| {
            let view = merge::view(resolved, branch);
            (
                view.types.clone().unwrap_or_default(),
                view.required.clone(),
                view.properties.keys().cloned().collect(),
            )
        })
        .collect();

    for (index, (types, required, properties)) in views.iter().enumerate() {
        for (other_types, other_required, other_properties) in views.iter().skip(index + 1) {
            let disjoint_types = !types.is_empty()
                && !other_types.is_empty()
                && types.intersection(other_types).next().is_none();
            let separated = required.difference(other_properties).next().is_some()
                || other_required.difference(properties).next().is_some();
            if !disjoint_types && !separated {
                return false;
            }
        }
    }
    true
}

fn report_unknown_types(view: &mut View, ctx: &mut Ctx, at: &JsonPointer) {
    for spelling in std::mem::take(&mut view.unknown_types) {
        if spelling == "file" {
            // The Swagger-era spelling for a binary payload, with a confident meaning.
            view.content_media_type
                .get_or_insert_with(|| "application/octet-stream".to_owned());
            view.types
                .get_or_insert_with(BTreeSet::new)
                .insert(TypeName::String);
            ctx.report(Diagnostic::new(
                BreakageClass::UnknownSchemaType,
                Action::Repair,
                at.child("type"),
                "`type: file` is the Swagger-era spelling for a binary payload; read it as a \
                 string with `contentMediaType: application/octet-stream`",
            ));
            continue;
        }
        ctx.report(Diagnostic::new(
            BreakageClass::UnknownSchemaType,
            Action::Degrade,
            at.child("type"),
            "`type` is not one of the seven JSON Schema types; the value is held as-is and typed \
             as arbitrary JSON",
        ));
    }
}

fn report_uninterpreted(view: &View, ctx: &mut Ctx, at: &JsonPointer) {
    if view.uninterpreted.is_empty() {
        return;
    }
    let keywords: Vec<String> = view
        .uninterpreted
        .iter()
        .map(|keyword| format!("`{keyword}`"))
        .collect();
    degrade(
        ctx,
        at,
        BreakageClass::UnsupportedConstruct,
        &format!(
            "{} narrow which values are valid in a way no Rust type expresses; the generated type \
             accepts more than the document allows",
            keywords.join(", ")
        ),
    );
}

fn degrade(ctx: &mut Ctx, at: &JsonPointer, class: BreakageClass, detail: &str) {
    ctx.report(Diagnostic::new(class, Action::Degrade, at.clone(), detail));
}

/// The docs a shape carries, for the contract layer.
pub(crate) fn docs(resolved: &ResolvedDocument, key: &ShapeKey) -> Docs {
    merge::view(resolved, key).docs
}
