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

use super::merge::{self, Discriminated, View};
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

/// The union table.
///
/// The rows, in the order the corpus makes them worth having: nullable emulation (83% of every
/// `anyOf` in the corpus), an enumeration of constants, one branch, and then the two ways a real
/// union is carried. Structural matching is preferred wherever it is *sound*, because it costs the
/// variants nothing; a declared discriminator is consumed as a serde tag only where it is not, and
/// a union that is neither soundly structural nor soundly tagged degrades to arbitrary JSON.
///
/// The order is the whole policy in one sentence: **a union is matched by shape when shape decides
/// it, by its tag when only the tag decides it, and by neither when nothing does.**
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
            } else {
                match ambiguity(resolved, &rest) {
                    // Shape decides it: an untagged enum is exact, and every variant keeps every
                    // property the document gave it.
                    None => Shape::Union(Union {
                        variants: rest
                            .into_iter()
                            .map(|key| Variant {
                                shape: ShapeRef::Key(key),
                                tag: None,
                            })
                            .collect(),
                        tag: None,
                    }),
                    Some(reason) => tagged(resolved, view, rest, &reason, ctx, at),
                }
            }
        }
    };

    if nulls > 0 {
        return Shape::Optional(ShapeRef::Inline(Box::new(inner)));
    }
    inner
}

/// A union whose variants shape alone cannot tell apart: carry the tag, or give up.
///
/// Proposed rather than decided here. Whether the tag can actually be consumed depends on facts no
/// single schema knows — chiefly whether the variant types are used anywhere but in this union,
/// since consuming the tag costs them the property that carries it — so [`super::discriminate`]
/// settles it once every shape exists, and demotes this back to a degradation when it cannot.
fn tagged(
    resolved: &ResolvedDocument,
    view: &View,
    branches: Vec<ShapeKey>,
    reason: &str,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let Some(declared) = &view.discriminator else {
        // No discriminator and no way to tell the variants apart. An untagged enum here picks
        // whichever variant happens to deserialize first, which is the predecessor's "wild union
        // semantics" breakage class and the one forbidden failure mode.
        degrade(
            ctx,
            at,
            BreakageClass::WildUnion,
            &format!(
                "this union declares no discriminator and {reason}, so nothing decides which \
                 branch a payload is; the value is typed as arbitrary JSON rather than matched \
                 against whichever branch happens to parse first"
            ),
        );
        return Shape::Any;
    };

    let variants = branches
        .into_iter()
        .map(|key| {
            let tag = tag_value(resolved, declared, &key);
            Variant {
                shape: ShapeRef::Key(key),
                tag,
            }
        })
        .collect();
    Shape::Union(Union {
        variants,
        tag: Some(declared.property.clone()),
    })
}

/// The name a discriminated union knows one branch by.
///
/// The resolution order OpenAPI defines: an explicit `mapping` entry naming this branch wins, and
/// otherwise the branch's own component name stands in. A mapping value is either a `$ref` or the
/// bare component name, and both spellings mean the same position.
fn tag_value(
    resolved: &ResolvedDocument,
    declared: &Discriminated,
    branch: &ShapeKey,
) -> Option<String> {
    let addresses: Vec<String> = branch
        .parts()
        .iter()
        .map(|&id| resolved.schemas().address(id).to_string())
        .collect();
    for (name, target) in &declared.mapping {
        if addresses.iter().any(|address| address == &mapped(target)) {
            return Some(name.clone());
        }
    }
    // No explicit entry: the component's own name is the implicit mapping.
    component_name(&addresses)
}

/// The document position a discriminator mapping value names.
fn mapped(target: &str) -> String {
    match target.strip_prefix('#') {
        Some(pointer) => pointer.to_owned(),
        // The shorthand OpenAPI allows: a bare name means a schema component.
        None => format!("/components/schemas/{target}"),
    }
}

/// The `components.schemas` name among a key's addresses, if one is there.
fn component_name(addresses: &[String]) -> Option<String> {
    addresses
        .iter()
        .find_map(|address| address.strip_prefix("/components/schemas/"))
        .filter(|name| !name.contains('/'))
        .map(ToOwned::to_owned)
}

/// Why matching these branches by shape alone would be unsound, if it would be.
///
/// The failure this rules out is precise: an untagged enum tries its variants in order and takes
/// the first that deserializes, so it is wrong exactly when a payload meant for one branch is
/// silently accepted by an earlier one *and read as something else*. That is a real risk between
/// two objects, where an open struct accepts a payload with members it does not declare and drops
/// them; it is not a risk between, say, a string and an integer, where the branch that parses is
/// the branch the payload is.
///
/// So the test is applied where the loss can happen and nowhere else. Being narrow here is not
/// leniency: it is what keeps the sound cases — 754 disjoint-type `anyOf`s in the corpus — from
/// degrading to `serde_json::Value` for a hazard that does not apply to them.
fn ambiguity(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> Option<String> {
    ambiguity_at(resolved, branches, 0)
}

/// How far into nested arrays the ambiguity test follows a union before giving up.
///
/// A union of arrays is ambiguous exactly when its element types are, so the test recurses — but a
/// document can nest arbitrarily and a recursive schema can nest forever, so the depth is capped
/// and the cap answers "ambiguous", which is the direction that degrades rather than guesses.
const MAX_AMBIGUITY_DEPTH: usize = 4;

fn ambiguity_at(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
    depth: usize,
) -> Option<String> {
    let mut leaves = Vec::new();
    if let Some(reason) = flatten(resolved, branches, depth, &mut leaves) {
        return Some(reason);
    }
    let mut views: Vec<View> = leaves
        .iter()
        .map(|branch| merge::view(resolved, branch))
        .collect();

    // A branch that constrains nothing accepts every payload its siblings do — but an untagged
    // enum tries its variants in declaration order, so one sitting *last* is a catch-all that only
    // ever runs when everything else has failed. That is a shape real documents write on purpose
    // ("this list of things, or an untyped list"), and it loses nothing. Anywhere else the same
    // branch swallows the ones after it, which loses everything.
    if views.last().is_some_and(|view| kind_of(view).is_none()) {
        views.pop();
    }
    for (index, view) in views.iter().enumerate() {
        if kind_of(view).is_none() {
            return Some(
                "one of its branches says nothing about the shape of its own payload and is not \
                 the last, so it accepts the payloads meant for every branch after it"
                    .to_owned(),
            );
        }
        for other in views.iter().skip(index + 1) {
            let (Some(left), Some(right)) = (kind_of(view), kind_of(other)) else {
                continue;
            };
            if left != right {
                continue;
            }
            match left {
                Kind::Object => {
                    if !separated(resolved, view, other) {
                        return Some(
                            "two of its branches are objects that no required property and no \
                             constant value tells apart"
                                .to_owned(),
                        );
                    }
                }
                // Two lists are told apart by what is in them, which is the same question one
                // level down.
                Kind::Array => {
                    let (Some(left), Some(right)) = (view.items.clone(), other.items.clone())
                    else {
                        return Some(
                            "two of its branches are lists and at least one does not say what is \
                             in it"
                                .to_owned(),
                        );
                    };
                    if left != right
                        && let Some(reason) =
                            ambiguity_at(resolved, &[left, right], depth.saturating_add(1))
                    {
                        return Some(reason);
                    }
                }
                // Two branches of one scalar kind read the same bytes into the same Rust type, so
                // whichever matches first is a faithful reading and nothing is lost.
                Kind::String | Kind::Number | Kind::Boolean | Kind::Null => {}
            }
        }
    }
    None
}

/// Expand every branch that is itself a union, so the comparison is between real payload shapes.
///
/// `oneOf: [A, {oneOf: [B, C]}]` accepts exactly what `oneOf: [A, B, C]` accepts, and serde's
/// nested untagged enums match it that way too — so the branch that matters is the leaf, and
/// treating the nested union as one opaque branch would report an ambiguity where the question had
/// simply not been asked yet.
///
/// Duplicates are dropped rather than compared: two branches that are the same shape read a
/// payload into the same value, so whichever matches is the right answer, and comparing a shape
/// with itself would find no separation and call a union ambiguous for being repetitive.
fn flatten(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
    depth: usize,
    out: &mut Vec<ShapeKey>,
) -> Option<String> {
    if depth > MAX_AMBIGUITY_DEPTH {
        return Some("its branches nest deeper than the shape comparison follows".to_owned());
    }
    for branch in branches {
        let view = merge::view(resolved, branch);
        match view.union.as_deref() {
            Some(inner) if !inner.is_empty() => {
                if let Some(reason) = flatten(resolved, inner, depth.saturating_add(1), out) {
                    return Some(reason);
                }
            }
            _ => {
                if !out.contains(branch) {
                    out.push(branch.clone());
                }
            }
        }
    }
    None
}

/// The JSON kind a branch's payloads have, when the branch says enough to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}

/// Which kind a view describes, by the same reading [`core`] and [`untyped`] give it.
///
/// `None` means the branch constrains nothing that decides a kind — which is the case that makes a
/// union unmatched, so it is deliberately not folded into some default.
fn kind_of(view: &View) -> Option<Kind> {
    if view.union.is_some() {
        // A branch that is itself a union has no single kind, and unfolding it here would be a
        // second, quieter copy of this whole function.
        return None;
    }
    let named: Vec<&TypeName> = view
        .types
        .iter()
        .flatten()
        .filter(|name| **name != TypeName::Null)
        .collect();
    match named.as_slice() {
        [TypeName::Object] => return Some(Kind::Object),
        [TypeName::Array] => return Some(Kind::Array),
        [TypeName::String] => return Some(Kind::String),
        [TypeName::Integer | TypeName::Number] => return Some(Kind::Number),
        [TypeName::Boolean] => return Some(Kind::Boolean),
        // Either `type` names several kinds, or it names none and the keywords have to say.
        _ if !named.is_empty() => return None,
        _ => {}
    }
    if view
        .types
        .as_ref()
        .is_some_and(|types| types.len() == 1 && types.contains(&TypeName::Null))
    {
        return Some(Kind::Null);
    }
    if !view.properties.is_empty()
        || view.additional.is_some()
        || view.additional_denied
        || view.uniform_pattern.is_some()
    {
        return Some(Kind::Object);
    }
    if view.items.is_some() || view.prefix_items.is_some() {
        return Some(Kind::Array);
    }
    if view.enumeration.is_some() || view.constant.is_some() {
        return Some(Kind::String);
    }
    if view.format.is_some() || view.content_encoding.is_some() || view.content_media_type.is_some()
    {
        return Some(Kind::String);
    }
    None
}

/// Whether two object branches can be told apart by a payload alone.
///
/// Two ways, both of which serde's untagged matching actually acts on: a property one branch
/// *requires* and the other does not declare at all, or a property both declare where the sets of
/// values each admits do not overlap. The second is what makes the common `const`-tagged union
/// exact without any tagging machinery — and it is worth having precisely because so many
/// documents write the tag as a constant *and* declare a discriminator beside it.
fn separated(resolved: &ResolvedDocument, left: &View, right: &View) -> bool {
    let missing = |required: &BTreeSet<String>, other: &View| {
        required
            .iter()
            .any(|name| !other.properties.contains_key(name))
    };
    if missing(&left.required, right) || missing(&right.required, left) {
        return true;
    }
    left.properties.iter().any(|(name, key)| {
        let Some(other) = right.properties.get(name) else {
            return false;
        };
        let (Some(here), Some(there)) = (
            admitted_values(resolved, key),
            admitted_values(resolved, other),
        ) else {
            return false;
        };
        here.is_disjoint(&there)
    })
}

/// The finite set of values a property admits, when it names one.
///
/// Only `const` and `enum` bound a property to a finite set; everything else admits values no
/// comparison can enumerate, and saying so is what keeps the disjointness test from claiming a
/// separation it has not established.
fn admitted_values(resolved: &ResolvedDocument, key: &ShapeKey) -> Option<BTreeSet<String>> {
    let view = merge::view(resolved, key);
    let values = match (&view.enumeration, &view.constant) {
        (Some(listed), _) => listed.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    Some(values.iter().map(ToString::to_string).collect())
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
