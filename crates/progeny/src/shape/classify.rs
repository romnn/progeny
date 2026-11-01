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

use std::collections::{BTreeMap, BTreeSet, btree_map};

use serde_json::Value;

use super::merge::{self, Discriminated, View};
use super::{
    Docs, Extra, Field, Format, IntType, Keyed, KeyedVariant, MAX_FIXED_ARRAY, Scalar, Shape,
    ShapeKey, ShapeRef, StringEnum, Struct, Tag, TagStyle, Union, Variant,
};
use crate::config::IntegerWidth;
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::{Schema, SchemaId, SchemaObject, TypeName};

/// The name `components.schemas` gave each shape, which is what a discriminator falls back to
/// when its mapping does not name a branch.
pub(super) type ComponentNames = BTreeMap<ShapeKey, String>;

/// Classify one key.
pub(crate) fn key(
    resolved: &ResolvedDocument,
    key: &ShapeKey,
    component_names: &ComponentNames,
    integers: IntegerWidth,
    ctx: &mut Ctx,
) -> Shape {
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
    inheritance_union(resolved, &mut view, key, ctx, &at);

    let shape = nullable(resolved, &view, key, component_names, ctx, &at);
    let shape = sized(&view, shape, integers, ctx, &at);
    report_uninterpreted(&view, &shape, ctx, &at);
    shape
}

/// An integer shape at the width its `format` names, where the configuration sizes integers by
/// format and the format can be followed.
///
/// Where it cannot, the integer keeps its wide type, and the reason is reported.
/// A `number` with an integer format stays a float, because the format sizes integers alone.
fn sized(
    view: &View,
    shape: Shape,
    integers: IntegerWidth,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    if integers == IntegerWidth::Wide {
        return shape;
    }
    let size = |scalar: Scalar, ctx: &mut Ctx| match scalar {
        Scalar::Integer(wide) => match format_width(view) {
            Ok(Some(width)) => Scalar::Integer(width),
            Ok(None) => scalar,
            Err(reason) => {
                degrade(
                    ctx,
                    at,
                    BreakageClass::UnsupportedConstruct,
                    &format!("{reason}, so the integer is `{}`", wide.rust()),
                );
                scalar
            }
        },
        Scalar::Number => {
            if let Some(format) = view
                .format
                .as_deref()
                .filter(|format| IntType::of_format(format).is_some())
            {
                degrade(
                    ctx,
                    at,
                    BreakageClass::UnsupportedConstruct,
                    &format!(
                        "`format: {format}` names an integer width, but the schema is a `number`, \
                         so it is `f64`"
                    ),
                );
            }
            scalar
        }
        Scalar::Bool | Scalar::String => scalar,
    };
    // A format beside a union, or anything else that is no integer of this schema's own, has
    // nothing here to size.
    let unsized_format = |ctx: &mut Ctx| {
        if let Some(format) = view
            .format
            .as_deref()
            .filter(|format| IntType::of_format(format).is_some())
        {
            degrade(
                ctx,
                at,
                BreakageClass::UnsupportedConstruct,
                &format!(
                    "`format: {format}` names an integer width, but this schema is no integer of \
                     its own, such as a union, so the format sizes nothing"
                ),
            );
        }
    };
    match shape {
        Shape::Scalar(scalar) => Shape::Scalar(size(scalar, ctx)),
        Shape::Optional(ShapeRef::Inline(inner)) => match *inner {
            Shape::Scalar(scalar) => {
                Shape::Optional(ShapeRef::Inline(Box::new(Shape::Scalar(size(scalar, ctx)))))
            }
            other => {
                unsized_format(ctx);
                Shape::Optional(ShapeRef::Inline(Box::new(other)))
            }
        },
        other => {
            unsized_format(ctx);
            other
        }
    }
}

/// The integer type a view's `format` names, `None` where it names none, or why it cannot be
/// followed: a format progeny does not know, parts that disagree, or a bound or listed value the
/// width cannot hold.
fn format_width(view: &View) -> Result<Option<IntType>, String> {
    let Some(format) = view.format.as_deref() else {
        return Ok(None);
    };
    if view.formats_disagree {
        return Err(format!(
            "the parts of this schema name different `format`s, `{format}` among them"
        ));
    }
    let Some(width) = IntType::of_format(format) else {
        return Err(format!(
            "`format: {format}` names no integer width progeny knows"
        ));
    };
    let (least, greatest) = width.range();
    let holds = |bound: Option<merge::Bound>| match bound {
        None => true,
        Some(merge::Bound::At(value)) => (least..=greatest).contains(&value),
        Some(merge::Bound::Unknown) => false,
    };
    if !holds(view.lower) || !holds(view.upper) {
        return Err(format!(
            "`format: {format}` names `{}`, but a bound of the schema reaches past it",
            width.rust()
        ));
    }
    let listed = listed_values(view).unwrap_or_default();
    if listed
        .iter()
        .any(|value| !value.is_null() && !holds_integer(width, value))
    {
        return Err(format!(
            "`format: {format}` names `{}`, but a value the schema lists does not fit it",
            width.rust()
        ));
    }
    Ok(Some(width))
}

/// Whether an integer type holds a value: a number that is whole and in its range, compared by
/// what it is, so a listed `-1.0` is the payload `-1`.
fn holds_integer(width: IntType, value: &Value) -> bool {
    let (least, greatest) = width.range();
    let Value::Number(number) = value else {
        return false;
    };
    merge::integer_value(number).is_some_and(|value| (least..=greatest).contains(&value))
}

/// The inheritance spelling of a discriminated union, turned into the union it describes.
///
/// OpenAPI's own polymorphism guide writes a union without writing `oneOf`: the base schema carries
/// the `discriminator` and its `mapping`, and each variant `allOf`s that base. Read literally the
/// base is a struct with one property and the mapping says nothing at all, so a `Vermietet` payload
/// deserializes into a value holding its tag and none of its own members — output that is wrong
/// without being visibly wrong, which is the failure mode progeny does not permit. The mapping is
/// the document naming its own variants, so it is read as the union it names.
///
/// **Only for the schema that declares the discriminator**, which is what one part means. A variant
/// merges the base into its own key, so its view carries the discriminator too; synthesizing there
/// would make every variant a union of its own siblings.
fn inheritance_union(
    resolved: &ResolvedDocument,
    view: &mut View,
    key: &ShapeKey,
    ctx: &mut Ctx,
    at: &JsonPointer,
) {
    let Subtypes { branches, unmapped } = subtypes(resolved, view, key);
    // A union flattened beside its schema's members is that schema's union again, and the schema
    // has reported what its mapping says already.
    let report = !key.is_branches();
    for target in unmapped.into_iter().filter(|_| report) {
        degrade(
            ctx,
            at,
            BreakageClass::DiscriminatorEdgeCase,
            &format!(
                "this discriminator maps a tag to `{target}`, which names no schema in this \
                 document, so that variant is not one of the union's"
            ),
        );
    }
    // Fewer than two variants is not a union, and the literal reading — the base as it is written —
    // loses nothing that a one-armed enum would keep.
    if branches.len() < 2 {
        return;
    }
    if !report {
        view.union = Some(branches);
        return;
    }
    ctx.report(Diagnostic::new(
        BreakageClass::InheritanceDiscriminator,
        Action::Repair,
        at.clone(),
        format!(
            "this schema declares a `discriminator` with a `mapping` and no `oneOf`, which is \
             how OpenAPI's polymorphism guide writes a union; read as the union of the {} \
             schemas the mapping names, because the literal reading is a struct holding the \
             tag and nothing each variant adds",
            branches.len()
        ),
    ));
    view.union = Some(branches);
}

/// Peel `null` off the front, since every other arm is about a value that is not null.
fn nullable(
    resolved: &ResolvedDocument,
    view: &View,
    key: &ShapeKey,
    component_names: &ComponentNames,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
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

    let core = core(
        resolved,
        view,
        key,
        others.as_ref(),
        component_names,
        ctx,
        at,
    );
    if declares_null(view) {
        return Shape::Optional(ShapeRef::Inline(Box::new(core)));
    }
    core
}

/// Whether a schema's own `type` or `enum` admits `null`, which makes its shape an `Option`.
///
/// An `enum` widened with `null` is nullable too, which is how 3.0's `nullable: true` beside an
/// enum arrives here after normalization.
fn declares_null(view: &View) -> bool {
    view.types
        .as_ref()
        .is_some_and(|types| types.contains(&TypeName::Null))
        || view
            .enumeration
            .as_ref()
            .is_some_and(|values| values.iter().any(Value::is_null))
}

/// Everything but nullability.
fn core(
    resolved: &ResolvedDocument,
    view: &View,
    key: &ShapeKey,
    types: Option<&BTreeSet<TypeName>>,
    component_names: &ComponentNames,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    if let Some(branches) = &view.union {
        // Members beside the union that no branch reads are a struct with the union flattened.
        if let Some(structure) = flattened(resolved, view, key, branches) {
            return structure;
        }
        let shape = match &view.discriminator {
            // A family's subtypes all hold the base's members, so their shapes say little, and the
            // tag the base declares is what the document dispatches by, even with one subtype.
            Some(declared) if super::inheritance::heads_family(resolved, key) => {
                tagged_family(resolved, declared, branches, component_names)
            }
            _ => union(resolved, view, branches, component_names, ctx, at),
        };
        return keep_own_members(resolved, view, shape, ctx, at);
    }
    if let Some(values) = string_values(view, ctx, at) {
        return Shape::StringEnum(StringEnum::listed(values));
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
        Some(TypeName::Array) => array(resolved, view, ctx, at),
        Some(TypeName::String) => string(view),
        Some(TypeName::Integer) => Shape::Scalar(Scalar::Integer(IntType::wide(!view.unsigned))),
        Some(TypeName::Number) => Shape::Scalar(Scalar::Number),
        Some(TypeName::Boolean) => Shape::Scalar(Scalar::Bool),
        Some(TypeName::Null) => Shape::Null,
        // `type` names something progeny does not know; the diagnostic is already recorded.
        Some(TypeName::Other(_)) => Shape::Any,
        // A document that omits `type` but says what the members are means an object, and the
        // corpus is full of both spellings. Guessing from the other keywords is not a repair: it
        // is what the schema says.
        None => untyped(resolved, view, ctx, at),
    }
}

fn untyped(resolved: &ResolvedDocument, view: &View, ctx: &mut Ctx, at: &JsonPointer) -> Shape {
    if !view.properties.is_empty() || view.additional.is_some() || view.additional_denied {
        return object(view);
    }
    if view.items.is_some() || view.prefix_items.is_some() {
        return array(resolved, view, ctx, at);
    }
    if view.uniform_pattern.is_some() {
        return object(view);
    }
    if let Some(scalar) = enumerated_scalar(view) {
        return scalar;
    }
    if view.format.is_some() || view.content_encoding.is_some() || view.content_media_type.is_some()
    {
        return string(view);
    }
    Shape::Any
}

/// The scalar a bare `enum`/`const` of one non-string kind names.
///
/// `{"enum": [1, 2, 3]}` says its values are numbers as surely as `type: integer` would, and real
/// documents write it both ways. Reading it as arbitrary JSON loses the type twice over: the field
/// stops being a number, and — because `serde_json::Value` accepts every payload — the branch
/// swallows its siblings wherever it sits in a union.
///
/// String enumerations never reach here: they have a named Rust type of their own and [`core`]
/// takes them before the `type` keyword is consulted at all. Only the narrowing to the listed
/// values is lost, which is the sound direction and the one [`string_values`] already documents.
fn enumerated_scalar(view: &View) -> Option<Shape> {
    match enumerated_kind(view)? {
        Kind::Number => Some(Shape::Scalar(if integral(view) {
            Scalar::Integer(IntType::wide(!view.unsigned))
        } else {
            Scalar::Number
        })),
        Kind::Boolean => Some(Shape::Scalar(Scalar::Bool)),
        Kind::String | Kind::Array | Kind::Object | Kind::Null => None,
    }
}

/// Whether every listed value is a whole number, which is what separates the two numeric scalars.
fn integral(view: &View) -> bool {
    let declared = match (&view.enumeration, &view.constant) {
        (Some(values), _) => values.as_slice(),
        (None, Some(value)) => std::slice::from_ref(value),
        (None, None) => return false,
    };
    declared
        .iter()
        .filter(|value| !value.is_null())
        .all(|value| value.as_i64().is_some() || value.as_u64().is_some())
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
    Shape::Struct(Struct {
        fields,
        extra,
        flattened: None,
    })
}

fn array(resolved: &ResolvedDocument, view: &View, ctx: &mut Ctx, at: &JsonPointer) -> Shape {
    let item = view.items.clone().map(ShapeRef::Key);
    if let Some(prefix) = &view.prefix_items {
        // An empty `prefixItems` constrains no position, so it is an array and not a tuple of
        // nothing — which would be the unit type and would serialize as `null`.
        if !prefix.is_empty() {
            // 2020-12: `items` beside a prefix constrains the elements *past* it. One that admits
            // no value — `false`, which is what draft-04's `additionalItems: false` normalizes to
            // — is exactly a fixed tuple. One that admits values means the document allows longer
            // instances than any fixed-arity Rust tuple accepts, and a generated type that
            // refuses instances the document allows would be wrong about the wire; this used to
            // be dropped with no diagnostic at all.
            if open_tuple(resolved, view) {
                degrade(
                    ctx,
                    at,
                    BreakageClass::UnsupportedConstruct,
                    "`prefixItems` is followed by an `items` that admits values, so instances may \
                     be longer than the prefix, which no fixed-arity tuple expresses",
                );
                return Shape::Any;
            }
            return Shape::Tuple {
                items: prefix.iter().cloned().map(ShapeRef::Key).collect(),
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

/// Whether a view is a `prefixItems` tuple whose `items` admits more elements, which [`array`]
/// degrades to arbitrary JSON.
fn open_tuple(resolved: &ResolvedDocument, view: &View) -> bool {
    view.prefix_items
        .as_ref()
        .is_some_and(|prefix| !prefix.is_empty())
        && view
            .items
            .as_ref()
            .is_some_and(|rest| !merge::view(resolved, rest).impossible)
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
        Some("ip") => Shape::Format(Format::Ip),
        Some("ipv4") => Shape::Format(Format::Ipv4),
        Some("ipv6") => Shape::Format(Format::Ipv6),
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
/// Before it degrades for want of a tag, a union whose branches include unions of their own is
/// matched by shape once more, with those read as their branches ([`spread_union`]).
///
/// The order is the whole policy in one sentence: **a union is matched by shape when shape decides
/// it, by its tag when only the tag decides it, and by neither when nothing does.**
fn union(
    resolved: &ResolvedDocument,
    view: &View,
    branches: &[ShapeKey],
    component_names: &ComponentNames,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let (reading, mut nullable) = Reading::of(resolved, branches);
    let inner = match reading {
        Reading::Empty => {
            return if nullable { Shape::Null } else { Shape::Any };
        }
        // `anyOf: [T, {type: null}]` — the nullable-emulation pattern, and the reason to look at
        // the branches at all before treating this as a union.
        Reading::One(key) => Shape::Alias(ShapeRef::Key(key)),
        Reading::Strings(strings) => Shape::StringEnum(strings),
        Reading::Variants(rest) => match told_apart(resolved, view, rest) {
            Ok(shape) => shape,
            Err((rest, reason)) => match spread_union(resolved, view, &rest) {
                Some((shape, null)) => {
                    nullable |= null;
                    shape
                }
                None => tagged(resolved, view, &rest, component_names, &reason, ctx, at),
            },
        },
    };

    if nullable {
        return Shape::Optional(ShapeRef::Inline(Box::new(inner)));
    }
    inner
}

/// Two or more branches as the union they are told apart in by what they hold, or why they cannot
/// be, with the branches handed back.
///
/// Shape tells them apart when every payload is read by the branch it is meant for, and the
/// branches tell themselves apart when they hold a member keyed by their own name or a constant of
/// their own.
fn told_apart(
    resolved: &ResolvedDocument,
    view: &View,
    rest: Vec<ShapeKey>,
) -> Result<Shape, (Vec<ShapeKey>, String)> {
    if let Some(keyed) = keyed(resolved, view, &rest) {
        return Ok(Shape::Keyed(keyed));
    }
    if let Some(named) = constant_discriminator(resolved, &rest, None) {
        // The branches name themselves: read the tag, the way a declared discriminator is read,
        // rather than match by shape — which would rest on the constants, and a generated string
        // enum reads any string.
        return Ok(self_named(named));
    }
    match ambiguity(resolved, &rest) {
        // Shape decides it: an untagged enum is exact, and every variant keeps every property the
        // document gave it.
        None => Ok(Shape::Union(Union {
            variants: rest
                .into_iter()
                .map(|key| Variant {
                    shape: ShapeRef::Key(key),
                    tag: None,
                    aliases: Vec::new(),
                })
                .collect(),
            tag: None,
        })),
        Some(reason) => match partitioned(resolved, view, &rest) {
            Some(shape) => Ok(shape),
            None => Err((rest, reason)),
        },
    }
}

/// Branches told apart by a member each of them requires and lists the values of, where the
/// branches that list one value are told apart by shape.
///
/// `type: {enum: [string]}` in four branches, `type: {enum: [number, integer]}` in a fifth: a
/// payload is tried against the branches that list its value of the member, in order, so a branch
/// is only ever handed a payload whose value it lists.
/// By shape alone that would not hold, because the type generated for the member is an open enum,
/// which reads a value its schema refuses, so a branch listing `string` would take a payload
/// meant for one listing `number`.
/// Among the branches that list one value the member says nothing more, and they are compared as
/// [`swallows_payloads_of`] compares two objects, with the member set aside ([`takes_beside`]).
///
/// Only plain closed objects ([`plain_closed`]) that each require the member and list strings for
/// it are read this way, so every payload a branch is meant for names the branches to try, and
/// what a branch's payloads carry is what its schema declares.
/// A member every branch lists the same values for tells nothing apart and is passed over.
/// A declared or inherited discriminator says which branch each value is, so a union that has one
/// is read by it instead.
fn partitioned(resolved: &ResolvedDocument, view: &View, branches: &[ShapeKey]) -> Option<Shape> {
    if view.discriminator.is_some() || shared_base_discriminator(resolved, branches).is_some() {
        return None;
    }
    let views = branches
        .iter()
        .map(|branch| {
            let view = merge::view(resolved, branch);
            let object = plain_closed(resolved, branch)?;
            (!names_its_subtypes(resolved, &view, branch)).then_some((view, object))
        })
        .collect::<Option<Vec<_>>>()?;
    let (first, _) = views.first()?;
    'candidates: for name in &first.required {
        let mut listed: Vec<Vec<String>> = Vec::new();
        for (view, object) in &views {
            let Some(values) = object
                .properties
                .as_ref()
                .and_then(|properties| properties.get(name))
                .filter(|_| view.required.contains(name))
                .and_then(|&member| partition_values(resolved, member))
            else {
                continue 'candidates;
            };
            listed.push(values);
        }
        let sets: Vec<BTreeSet<&String>> = listed
            .iter()
            .map(|values| values.iter().collect())
            .collect();
        if sets.iter().all(|values| Some(values) == sets.first()) {
            continue;
        }
        // The branches that list one value are tried in order, so none may read a later one's.
        let values: BTreeSet<&String> = listed.iter().flatten().collect();
        for value in values {
            let sharing: Vec<&ShapeKey> = branches
                .iter()
                .zip(&listed)
                .filter(|(_, values)| values.contains(value))
                .map(|(branch, _)| branch)
                .collect();
            for (at, earlier) in sharing.iter().enumerate() {
                for later in sharing.iter().skip(at + 1) {
                    if takes_beside(resolved, earlier, later, name) {
                        continue 'candidates;
                    }
                }
            }
        }
        // Where no value is listed twice, nothing is shared, and the union is left as it was.
        let shared = listed.iter().enumerate().any(|(at, values)| {
            listed
                .iter()
                .skip(at + 1)
                .any(|other| values.iter().any(|value| other.contains(value)))
        });
        if !shared {
            continue;
        }
        let variants = branches
            .iter()
            .zip(listed)
            .map(|(branch, values)| {
                // The first spelling is the one written, and the rest are read too.
                let mut values = values.into_iter();
                Variant {
                    shape: ShapeRef::Key(branch.clone()),
                    tag: values.next(),
                    aliases: values.collect(),
                }
            })
            .collect();
        return Some(Shape::Union(Union {
            variants,
            tag: Some(Tag {
                property: name.clone(),
                style: TagStyle::Consumed,
                absent: None,
                null: None,
                shared: true,
            }),
        }));
    }
    None
}

/// Every value a member's schema admits, when that is a list of strings: each string an `enum` or
/// `const` of the member lists that the member's whole schema admits, in the order they are
/// listed.
///
/// `member` is the schema the branch writes for it, which may be a `$ref` to the one that lists
/// the values.
/// Admission is judged by every schema the member merges ([`super::listed::admits`]), so a
/// `const` beside a list and a `type` that admits no string leave out what they refuse.
/// A member has no such list when anywhere along its `$ref`s it combines schemas with `allOf`,
/// `anyOf`, `oneOf`, `not` or `if`, writes a `$dynamicRef`, which resolution ignores beside a
/// `$ref`, or lists an empty `enum`, which admits nothing.
/// Neither has one that may admit a value the judgement cannot settle or that admits `null`,
/// because the branches it names would then not be the ones whose schemas admit the value.
fn partition_values(resolved: &ResolvedDocument, member: SchemaId) -> Option<Vec<String>> {
    use super::listed::{Admission, admits};
    // Every schema along the member's references is inspected, the last one included, or the
    // member is refused.
    let mut at = member;
    let mut inspected = false;
    for _ in 0..=MAX_AMBIGUITY_DEPTH {
        let Schema::Object(object) = resolved.schema(at) else {
            return None;
        };
        let combines = object.all_of.is_some()
            || object.any_of.is_some()
            || object.one_of.is_some()
            || object.not.is_some()
            || object.if_schema.is_some()
            || object.dynamic_reference.is_some();
        if combines || object.enumeration.as_ref().is_some_and(Vec::is_empty) {
            return None;
        }
        let Some(target) = resolved.follow(at) else {
            inspected = true;
            break;
        };
        at = target;
    }
    if !inspected {
        return None;
    }
    let key = merge::key_of(resolved, member);
    if admits(resolved, &key, &Value::Null, 0) != Admission::No {
        return None;
    }
    let mut listed: Vec<&Value> = Vec::new();
    for &part in key.parts() {
        let Schema::Object(object) = resolved.schema(part.id()) else {
            continue;
        };
        listed.extend(object.enumeration.iter().flatten());
        listed.extend(&object.constant);
    }
    let mut values: Vec<String> = Vec::new();
    for value in listed {
        let text = value.as_str()?;
        match admits(resolved, &key, value, 0) {
            Admission::Yes if !values.iter().any(|known| known == text) => {
                values.push(text.to_owned());
            }
            Admission::Yes | Admission::No => {}
            Admission::Unknown => return None,
        }
    }
    (!values.is_empty()).then_some(values)
}

/// Whether the object branch tried first would take a payload of a later one that names, in
/// `member`, a value both list.
///
/// [`swallows_payloads_of`]'s reading of two objects, with `member` set aside where it asks
/// whether the earlier branch reads values its schema refuses: the payload's value of it is one
/// the earlier branch lists.
/// Only an earlier branch that reads such a payload as the later one would ([`reads_as_well`])
/// counts as faithful to it, because that judges the schemas the two types are generated from
/// rather than the types.
fn takes_beside(
    resolved: &ResolvedDocument,
    earlier: &ShapeKey,
    later: &ShapeKey,
    member: &str,
) -> bool {
    let (first, second) = (merge::view(resolved, earlier), merge::view(resolved, later));
    let mut beside = merge::view(resolved, earlier);
    beside.properties.remove(member);
    beside.required.remove(member);
    let faithful =
        reads_as_well(resolved, &first, &second, member) && !view_overreaches(resolved, &beside, 0);
    !faithful && !refuses(resolved, &first, &second, 0)
}

/// Whether a plain closed object's type refuses every payload of a later plain closed object,
/// judged by what the later one's schema guarantees its payloads carry and leave out.
///
/// A member the later one requires is on every payload, and the earlier one refuses it where it
/// does not declare it.
/// A member the later one does not declare is on none, so an earlier one that insists on it
/// refuses them all.
/// A member both declare and the later one requires refuses the payload where both are plain
/// closed objects and the earlier one's refuses the later one's in turn, as far as
/// [`MAX_AMBIGUITY_DEPTH`].
/// Nothing is concluded from the kinds the two members name, which the generated types need not
/// follow.
fn refuses(resolved: &ResolvedDocument, earlier: &View, later: &View, depth: usize) -> bool {
    if depth > MAX_AMBIGUITY_DEPTH {
        return false;
    }
    let carried =
        |name: &String| later.required.contains(name) && later.properties.contains_key(name);
    let undeclared = later
        .properties
        .keys()
        .any(|name| carried(name) && !earlier.properties.contains_key(name));
    let missing = earlier.required.iter().any(|name| {
        !later.properties.contains_key(name)
            && earlier
                .properties
                .get(name)
                .is_some_and(|key| !generates_option(resolved, key, 0))
    });
    undeclared
        || missing
        || earlier.properties.iter().any(|(name, key)| {
            let Some(other) = later.properties.get(name).filter(|_| carried(name)) else {
                return false;
            };
            plain_closed(resolved, key).is_some()
                && plain_closed(resolved, other).is_some()
                && refuses(
                    resolved,
                    &merge::view(resolved, key),
                    &merge::view(resolved, other),
                    depth + 1,
                )
        })
}

/// The one schema a key is, where it is one closed object and nothing more: `type: object`,
/// `additionalProperties: false`, its members exactly the ones it declares, and no keyword beside
/// them that admits or refuses anything else, so its payloads carry what it declares and its type
/// reads them as such.
///
/// It references nothing either, so its one part is everything it says.
/// A merge that stops short of a reference's schemas still leaves a key of one part, and
/// resolution ignores a `$dynamicRef` written beside a `$ref`.
fn plain_closed<'a>(resolved: &'a ResolvedDocument, key: &ShapeKey) -> Option<&'a SchemaObject> {
    let [part] = key.parts() else {
        return None;
    };
    let Schema::Object(object) = resolved.schema(part.id()) else {
        return None;
    };
    let closed = object
        .additional_properties
        .is_some_and(|rest| matches!(resolved.schema(rest), Schema::Bool(false)));
    // An object with no `type` of its own admits every other kind of value too, `null` among them.
    let object_type = object
        .types
        .as_ref()
        .is_some_and(|types| types.iter().all(|name| *name == TypeName::Object));
    let plain = closed
        && object_type
        && object.properties.is_some()
        && object.pattern_properties.is_none()
        && object.property_names.is_none()
        && object.unevaluated_properties.is_none()
        && object.dependent_schemas.is_none()
        && object.dependent_required.is_none()
        && object.min_properties.is_none()
        && object.max_properties.is_none()
        && object.not.is_none()
        && object.if_schema.is_none()
        && object.then_schema.is_none()
        && object.else_schema.is_none()
        && object.all_of.is_none()
        && object.any_of.is_none()
        && object.one_of.is_none()
        && object.enumeration.is_none()
        && object.constant.is_none()
        && object.discriminator.is_none()
        && resolved.follow(part.id()).is_none()
        && object.dynamic_reference.is_none();
    plain.then_some(object)
}

/// Whether a closed struct reads every payload of a later branch it accepts as that branch's own
/// type would.
///
/// It accepts only payloads whose members it declares, and each member the two share is the same
/// schema, required or not alike, so it becomes the same field and keeps what the later one keeps.
/// A member only the earlier one declares is never on a payload of a later closed struct.
/// `member` is set aside: the payloads compared name a value of it both list, which each reads as
/// that listed value.
fn reads_as_well(resolved: &ResolvedDocument, earlier: &View, later: &View, member: &str) -> bool {
    earlier.additional_denied
        && earlier
            .properties
            .iter()
            .filter(|(name, _)| name.as_str() != member)
            .all(|(name, key)| match later.properties.get(name) {
                Some(other) => {
                    earlier.required.contains(name) == later.required.contains(name)
                        && same_schema(resolved, key, other)
                }
                None => later.additional_denied,
            })
}

/// Whether two keys are the same schemas, written at the same or at different places.
fn same_schema(resolved: &ResolvedDocument, left: &ShapeKey, right: &ShapeKey) -> bool {
    left == right
        || (left.is_branches() == right.is_branches()
            && left.parts().len() == right.parts().len()
            && left
                .parts()
                .iter()
                .zip(right.parts())
                .all(|(&left, &right)| {
                    left.dispatches() == right.dispatches()
                        && same_written(resolved, left.id(), right.id(), 0)
                }))
}

/// Whether two schemas say the same, each keyword alike and each subschema in turn, as far as
/// [`MAX_AMBIGUITY_DEPTH`]; deeper than that they are taken to differ.
///
/// A `$ref` is compared as written and by the schema it reaches, so two references are the same
/// only where they reach one schema.
fn same_written(
    resolved: &ResolvedDocument,
    left: SchemaId,
    right: SchemaId,
    depth: usize,
) -> bool {
    if left == right {
        return true;
    }
    if depth > MAX_AMBIGUITY_DEPTH {
        return false;
    }
    match (resolved.schema(left), resolved.schema(right)) {
        (Schema::Bool(written), Schema::Bool(other)) => written == other,
        (Schema::Object(written), Schema::Object(other)) => {
            let (mut lefts, mut rights) = (Vec::new(), Vec::new());
            written.children(|id| lefts.push(id));
            other.children(|id| rights.push(id));
            written.same_apart_from_children(other)
                && resolved.follow(left) == resolved.follow(right)
                && lefts.len() == rights.len()
                && lefts
                    .into_iter()
                    .zip(rights)
                    .all(|(left, right)| same_written(resolved, left, right, depth + 1))
        }
        _ => false,
    }
}

/// A union whose branches are told apart once every nested union among them is read as its own
/// branches, and whether one of those was `null`.
///
/// An `anyOf` that is a branch of another union and declares nothing beside its branches admits
/// exactly what they do, so the outer union admits the same payloads with them in its place.
/// The type generated for the nested union holds every payload none of its branches fits, which
/// is what a later branch is meant for, and read as its branches it holds none.
/// Wherever it is referenced on its own, it keeps that type.
///
/// A declared or inherited discriminator decides the union as written, so nothing is spread there.
fn spread_union(
    resolved: &ResolvedDocument,
    view: &View,
    rest: &[ShapeKey],
) -> Option<(Shape, bool)> {
    if view.discriminator.is_some() || shared_base_discriminator(resolved, rest).is_some() {
        return None;
    }
    let mut spread = Vec::new();
    if !spread_into(resolved, rest, &mut BTreeSet::new(), &mut spread) {
        return None;
    }
    let (reading, nullable) = Reading::of(resolved, &spread);
    let shape = match reading {
        Reading::Empty => return None,
        Reading::One(key) => Shape::Alias(ShapeRef::Key(key)),
        Reading::Strings(strings) => Shape::StringEnum(strings),
        Reading::Variants(spread) => told_apart(resolved, view, spread).ok()?,
    };
    Some((shape, nullable))
}

/// Push `branches` onto `spread`, each nested union that declares nothing beside its branches as
/// those branches, and say whether there was one.
///
/// `open` holds the unions being spread, so a union that is a branch of itself stays one branch,
/// which [`ambiguity`] then compares as the type it becomes.
fn spread_into(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
    open: &mut BTreeSet<ShapeKey>,
    spread: &mut Vec<ShapeKey>,
) -> bool {
    let mut found = false;
    for branch in branches {
        match bare_union(resolved, branch) {
            Some(nested) if open.insert(branch.clone()) => {
                spread_into(resolved, &nested, open, spread);
                open.remove(branch);
                found = true;
            }
            _ => spread.push(branch.clone()),
        }
    }
    found
}

/// The branches of an `anyOf` that declares nothing but them, annotations aside.
///
/// A union with a discriminator, a type, members or any other keyword beside its branches admits
/// less than they do, or is read by its tag, so it is not one.
/// Neither is a `oneOf`, which refuses a payload two of its branches admit, and so leaves it to a
/// later branch of the outer union that its branches, read in its place, would take first.
fn bare_union(resolved: &ResolvedDocument, key: &ShapeKey) -> Option<Vec<ShapeKey>> {
    let one_of = key.parts().iter().any(|&part| {
        part.dispatches()
            && matches!(resolved.schema(part.id()), Schema::Object(object) if object.one_of.is_some())
    });
    if one_of {
        return None;
    }
    let view = merge::view(resolved, key);
    let bare = view.types.is_none()
        && view.enumeration.is_none()
        && view.constant.is_none()
        && view.properties.is_empty()
        && view.required.is_empty()
        && view.additional.is_none()
        && !view.additional_denied
        && view.uniform_pattern.is_none()
        && view.items.is_none()
        && view.prefix_items.is_none()
        && view.min_items.is_none()
        && view.max_items.is_none()
        && !view.unsigned
        && view.lower.is_none()
        && view.upper.is_none()
        && view.format.is_none()
        && !view.formats_disagree
        && view.content_encoding.is_none()
        && view.content_media_type.is_none()
        && view.discriminator.is_none()
        && !view.unions_collide
        && view.uninterpreted.is_empty()
        && view.conflict.is_none()
        && !view.impossible
        && view.unknown_types.is_empty();
    view.union.filter(|_| bare)
}

/// The subtypes a base schema's discriminator mapping names, and the mapped targets that name no
/// schema.
///
/// Empty unless `key` is the schema that declares the discriminator, for the reason
/// [`inheritance_union`] gives.
fn subtypes<'a>(resolved: &ResolvedDocument, view: &'a View, key: &ShapeKey) -> Subtypes<'a> {
    let mut branches: Vec<ShapeKey> = Vec::new();
    let mut unmapped: Vec<&str> = Vec::new();
    let Some(declared) = &view.discriminator else {
        return Subtypes { branches, unmapped };
    };
    if view.union.is_some() || key.parts().len() != 1 {
        return Subtypes { branches, unmapped };
    }
    for target in declared.mapping.values() {
        let Some(id) = resolved.discriminator_target(target) else {
            unmapped.push(target);
            continue;
        };
        let branch = merge::key_of(resolved, id);
        // A mapping naming the base itself would make the union one of its own variants, which is
        // a type that contains itself by value.
        if &branch != key && !branches.contains(&branch) {
            branches.push(branch);
        }
    }
    Subtypes { branches, unmapped }
}

/// What [`subtypes`] reads from a discriminator mapping.
struct Subtypes<'a> {
    branches: Vec<ShapeKey>,
    unmapped: Vec<&'a str>,
}

/// Whether a schema is the base of an inheritance union, which [`inheritance_union`] reads as the
/// union of its subtypes.
fn names_its_subtypes(resolved: &ResolvedDocument, view: &View, key: &ShapeKey) -> bool {
    subtypes(resolved, view, key).branches.len() >= 2
}

/// `{properties: P, anyOf: [A, B]}` as a struct of `P`'s members with the union of `A` and `B`
/// flattened beside them, when every member has exactly one reader.
///
/// The struct's fields take `P`'s members, and the union, classified apart under
/// [`ShapeKey::branches`], takes every other member.
/// That is sound only where no member could belong to both or to neither, so this declines, and
/// the union is read as [`keep_own_members`] reads it, wherever:
///
/// - there is one distinct branch, which is an alias rather than a union and flattens differently
///   under the derive;
/// - a branch is not an object of its own, or may be `null`, because only an object's members can
///   sit beside `P`'s;
/// - a branch declares or requires a member `P` declares or requires, or names one as its
///   discriminator, because the fields would take it before the branch saw it;
/// - `P` requires a member it does not declare, because no field would read it;
/// - `P` says what its other members are, with `additionalProperties` or `patternProperties`,
///   because the union reads those members too;
/// - the discriminator names a member of `P`, because the union could not read its own tag.
fn flattened(
    resolved: &ResolvedDocument,
    view: &View,
    key: &ShapeKey,
    branches: &[ShapeKey],
) -> Option<Shape> {
    if !flattens(resolved, view, branches) {
        return None;
    }
    let Shape::Struct(mut structure) = object(view) else {
        return None;
    };
    structure.flattened = Some(ShapeRef::Key(key.branches()));
    Some(Shape::Struct(structure))
}

/// Whether [`flattened`] reads a view's members and its union `branches` as a struct, by the
/// rules it lists.
///
/// A branch's own kind is asked only of a branch that declares no union, so two schemas whose
/// unions name each other never ask each other.
/// A view [`key`] degrades before it reads the union flattens nothing, because it becomes
/// arbitrary JSON.
fn flattens(resolved: &ResolvedDocument, view: &View, branches: &[ShapeKey]) -> bool {
    if view.impossible || view.conflict.is_some() || view.unions_collide {
        return false;
    }
    let own: BTreeSet<&str> = view
        .properties
        .keys()
        .chain(&view.required)
        .map(String::as_str)
        .collect();
    let undeclared_required = view
        .required
        .iter()
        .any(|name| !view.properties.contains_key(name));
    let describes_others = view.additional_denied
        || view.additional.is_some()
        || view.uniform_pattern.is_some()
        || view.uninterpreted.contains(&"patternProperties");
    let own_tag = view
        .discriminator
        .as_ref()
        .is_some_and(|declared| own.contains(declared.property.as_str()));
    let distinct: BTreeSet<&ShapeKey> = branches.iter().collect();
    if distinct.len() < 2
        || view.properties.is_empty()
        || undeclared_required
        || describes_others
        || own_tag
    {
        return false;
    }
    branches.iter().all(|branch| {
        let branch_view = merge::view(resolved, branch);
        let object = branch_view.union.is_none()
            && !names_its_subtypes(resolved, &branch_view, branch)
            && !declares_null(&branch_view)
            && kind_of(resolved, &branch_view) == Some(Kind::Object);
        // A discriminator a branch inherits names a member as surely as a property does.
        let shares = branch_view
            .properties
            .keys()
            .chain(&branch_view.required)
            .chain(
                branch_view
                    .discriminator
                    .iter()
                    .map(|declared| &declared.property),
            )
            .any(|name| own.contains(name.as_str()));
        object && !shares
    })
}

/// The union's shape, or arbitrary JSON when that shape would drop members declared beside it.
///
/// `{properties: P, anyOf: [A, B]}` means `P` and one of `A` or `B`, but the union is generated from
/// the branches alone.
/// A member of `P` that a branch's struct has no field for is read past and lost when the value is
/// written back, so the union is kept only when every branch carries every member the schema
/// names beside it, and keeps the extra members its `additionalProperties` describes.
/// A tag the union reads is exempt, because the enum writes it back itself.
fn keep_own_members(
    resolved: &ResolvedDocument,
    view: &View,
    shape: Shape,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let inner = match &shape {
        Shape::Optional(ShapeRef::Inline(inner)) => inner.as_ref(),
        other => other,
    };
    let (variants, tag): (Vec<&ShapeKey>, Option<&str>) = match inner {
        Shape::Alias(ShapeRef::Key(key)) => (vec![key], None),
        Shape::Union(union) => (
            union
                .variants
                .iter()
                .filter_map(|variant| match &variant.shape {
                    ShapeRef::Key(key) => Some(key),
                    ShapeRef::Inline(_) => None,
                })
                .collect(),
            union.tag.as_ref().map(|tag| tag.property.as_str()),
        ),
        // Strings carry no members, and arbitrary JSON keeps them all.
        // A keyed union is read so only where nothing is declared beside it ([`keyed`]).
        _ => return shape,
    };
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    for key in variants {
        dropped.extend(dropped_members(resolved, view, key, 0));
    }
    if let Some(tag) = tag {
        dropped.remove(&format!("`{tag}`"));
    }
    if dropped.is_empty() {
        return shape;
    }
    let dropped: Vec<String> = dropped.into_iter().collect();
    degrade(
        ctx,
        at,
        BreakageClass::UnsupportedConstruct,
        &format!(
            "this schema declares {} beside its `oneOf`/`anyOf`, and a branch does not keep them, \
             so that branch would drop them; the value is typed as arbitrary JSON so that no member \
             is lost",
            dropped.join(", ")
        ),
    );
    Shape::Any
}

/// How deep into nested members the test for dropped members follows them before giving up.
///
/// A recursive schema nests forever, so the depth is capped, and the cap answers "dropped",
/// which is the direction that degrades rather than loses members.
const MAX_MEMBER_DEPTH: usize = 4;

/// The members `source` declares that a value read through `destination`'s type loses, each named
/// for the diagnostic.
///
/// A destination that is not an object loses nothing, because a value it accepts has no members.
fn dropped_members(
    resolved: &ResolvedDocument,
    source: &View,
    destination: &ShapeKey,
    depth: usize,
) -> Vec<String> {
    let target = merge::view(resolved, destination);
    let union = target.union.is_some() || names_its_subtypes(resolved, &target, destination);
    if !union && kind_of(resolved, &target) != Some(Kind::Object) {
        return Vec::new();
    }
    let keeps =
        |member: Option<&ShapeKey>| !union && keeps_undeclared(resolved, &target, member, depth);
    let declared = source
        .properties
        .iter()
        .map(|(name, key)| (name, Some(key)));
    let required = source
        .required
        .iter()
        .filter(|name| !source.properties.contains_key(*name))
        .map(|name| (name, None));
    let mut dropped: Vec<String> = declared
        .chain(required)
        .filter(|(name, member)| match target.properties.get(*name) {
            Some(own) => !lands_intact(resolved, *member, Some(own), depth),
            None => !keeps(*member),
        })
        .map(|(name, _)| format!("`{name}`"))
        .collect();
    // Extra members land in the capture or in a field the source does not name.
    // A pattern also covers the names `properties` lists, so its members may land in any field.
    let extras = [
        (source.additional.as_ref(), false, "additionalProperties"),
        (source.uniform_pattern.as_ref(), true, "patternProperties"),
    ];
    for (extra, covers_named, keyword) in extras {
        let Some(extra) = extra else {
            continue;
        };
        let fields_keep = target
            .properties
            .iter()
            .filter(|(name, _)| covers_named || !source.properties.contains_key(*name))
            .all(|(_, own)| lands_intact(resolved, Some(extra), Some(own), depth));
        if !keeps(Some(extra)) || !fields_keep {
            dropped.push(format!("the members its `{keyword}` describes"));
        }
    }
    dropped
}

/// Whether an object `destination` keeps a member it does not declare, whose value `member`
/// describes.
///
/// It keeps it only in a map or a typed capture the value lands in intact.
fn keeps_undeclared(
    resolved: &ResolvedDocument,
    destination: &View,
    member: Option<&ShapeKey>,
    depth: usize,
) -> bool {
    let capture = destination
        .additional
        .as_ref()
        .or(destination.uniform_pattern.as_ref());
    !destination.additional_denied
        && (destination.properties.is_empty() || capture.is_some())
        && lands_intact(resolved, member, capture, depth)
}

/// Whether a member value `member` describes survives a round trip through `destination`.
///
/// `None` stands for a value the document says nothing about, or for arbitrary JSON.
/// The value survives a destination of the same schema or arbitrary JSON, a destination that
/// keeps every member and element it declares, or any destination when it is a scalar or
/// declares nothing to lose.
fn lands_intact(
    resolved: &ResolvedDocument,
    member: Option<&ShapeKey>,
    destination: Option<&ShapeKey>,
    depth: usize,
) -> bool {
    let (Some(member), Some(destination)) = (member, destination) else {
        return true;
    };
    if member == destination {
        return true;
    }
    if depth >= MAX_MEMBER_DEPTH {
        return false;
    }
    let (source, target) = (
        merge::view(resolved, member),
        merge::view(resolved, destination),
    );
    let (source_union, target_union) = (
        source.union.is_some() || names_its_subtypes(resolved, &source, member),
        target.union.is_some() || names_its_subtypes(resolved, &target, destination),
    );
    let declares_nothing = !source_union
        && source.properties.is_empty()
        && source.required.is_empty()
        && source.additional.is_none()
        && source.uniform_pattern.is_none()
        && source.items.is_none()
        && source.prefix_items.is_none();
    if declares_nothing || (!target_union && kind_of(resolved, &target).is_none()) {
        return true;
    }
    if source_union || target_union {
        return false;
    }
    match (kind_of(resolved, &source), kind_of(resolved, &target)) {
        (Some(Kind::String | Kind::Number | Kind::Boolean | Kind::Null), _) => true,
        (Some(Kind::Object), Some(Kind::Object)) => {
            dropped_members(resolved, &source, destination, depth + 1).is_empty()
        }
        (Some(Kind::Array), Some(Kind::Array)) => {
            source.prefix_items.is_none()
                && target.prefix_items.is_none()
                && lands_intact(
                    resolved,
                    source.items.as_ref(),
                    target.items.as_ref(),
                    depth + 1,
                )
        }
        _ => false,
    }
}

/// What a union's branches amount to once `null` and repeated branches are set aside.
///
/// The one reading of a union's branches, shared by [`union`], which builds the shape, and by
/// [`swallows_payloads_of`], which has to compare a nested union as that same shape.
enum Reading {
    /// No branch but `null`, or none at all.
    Empty,
    /// One branch, which the union is an alias or an `Option` of.
    One(ShapeKey),
    /// Branches that only list strings, or read any string, which read as one string enum
    /// ([`folded_strings`]).
    Strings(StringEnum),
    /// Two or more branches a payload has to be told apart between.
    Variants(Vec<ShapeKey>),
}

impl Reading {
    /// The reading of `branches`, and whether one of them was `null`.
    fn of(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> (Self, bool) {
        let mut nullable = false;
        let mut rest: Vec<ShapeKey> = Vec::new();
        for branch in branches {
            if is_null(resolved, branch) {
                nullable = true;
            } else if !rest.contains(branch) {
                rest.push(branch.clone());
            }
        }
        let reading = match rest.len() {
            0 => Self::Empty,
            1 => rest.pop().map_or(Self::Empty, Self::One),
            _ => match folded_strings(resolved, &rest) {
                // A catch-all `type: [string, null]` admits `null` as surely as a `null` branch.
                Some((strings, null)) => {
                    nullable |= null;
                    Self::Strings(strings)
                }
                None => Self::Variants(rest),
            },
        };
        (reading, nullable)
    }
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
    branches: &[ShapeKey],
    component_names: &ComponentNames,
    reason: &str,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let inherited = view
        .discriminator
        .is_none()
        .then(|| shared_base_discriminator(resolved, branches))
        .flatten();
    let Some(declared) = view.discriminator.as_ref().or(inherited.as_ref()) else {
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
    dispatched(resolved, declared, branches, component_names)
}

/// The union a family's base declares, read by its declared tag however many subtypes it lists.
///
/// Each subtype is named as [`union`] names the branches of any discriminated union: by the
/// constants every subtype binds the tag property to where those tell them apart, reading every
/// spelling and a missing or `null` tag as [`constant_discriminator`] does, and otherwise as
/// [`tagged`] names them, by the mapping and then by the component name.
fn tagged_family(
    resolved: &ResolvedDocument,
    declared: &Discriminated,
    branches: &[ShapeKey],
    component_names: &ComponentNames,
) -> Shape {
    let mut subtypes: Vec<ShapeKey> = Vec::new();
    for branch in branches {
        if !subtypes.contains(branch) {
            subtypes.push(branch.clone());
        }
    }
    match constant_discriminator(resolved, &subtypes, Some(&declared.property)) {
        Some(named) => self_named(named),
        None => dispatched(resolved, declared, &subtypes, component_names),
    }
}

/// A union read by a declared discriminator, each branch a variant named by the mapping or, where
/// the mapping does not name it, by its component name.
fn dispatched(
    resolved: &ResolvedDocument,
    declared: &Discriminated,
    branches: &[ShapeKey],
    component_names: &ComponentNames,
) -> Shape {
    let variants = branches
        .iter()
        .map(|key| Variant {
            shape: ShapeRef::Key(key.clone()),
            tag: tag_value(resolved, declared, component_names, key),
            aliases: Vec::new(),
        })
        .collect();
    Shape::Union(Union {
        variants,
        // Always proposed as consumed: [`super::discriminate`] may settle on carried instead,
        // once it can see how the variant types are used elsewhere.
        tag: Some(Tag::consumed(declared.property.clone())),
    })
}

/// The property every branch binds to string constants of its own, with each branch's
/// constants, when there is such a property.
///
/// `type: {enum: [creation]}` in one branch, `type: {enum: [update]}` in the next: a union of
/// objects that name themselves this way is discriminated by that property whether or not the
/// description spells `discriminator`, and is read the same way, by the tag's text.
/// It cannot be read by shape: the separation would rest on the constants, and a generated
/// string enum is open — it reads any string, so under it every branch fits every payload and
/// the first one wins.
///
/// A branch may list several spellings of its name, as anthropic's tool search tool lists a dated
/// one and an alias.
/// A property that binds every branch to one constant is preferred over one where a branch lists
/// several, so a union whose tag is spelled once per branch keeps that tag.
/// Within each of those two readings the first qualifying property in the first branch's wire
/// order is the tag, so the answer does not turn on which other branch is listed first.
///
/// The tag does not have to be on every payload.
/// One branch that leaves the property optional, where every other branch requires it, is what a
/// payload without it is, which is how anthropic's custom tool is sent.
/// One branch that admits `null` there is likewise what a payload with `null` in it is.
/// With two such branches the payload could be either, so it names no variant.
///
/// Where `tag` names the property, only that property is read, and one branch is enough: a union
/// declaring its tag says which property it is, so even a lone branch's constants spell it.
fn constant_discriminator(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
    tag: Option<&str>,
) -> Option<SelfNamed> {
    let views: Vec<(&ShapeKey, View)> = branches
        .iter()
        .map(|branch| (branch, merge::view(resolved, branch)))
        .collect();
    let fewest = if tag.is_some() { 1 } else { 2 };
    if views.len() < fewest
        || views
            .iter()
            .any(|(_, view)| kind_of(resolved, view) != Some(Kind::Object))
    {
        return None;
    }
    let (_, first) = views.first()?;
    // One constant per branch first, then several, so the stricter reading wins where both fit.
    for several in [false, true] {
        let candidates = first
            .properties
            .keys()
            .filter(|name| tag.is_none_or(|tag| tag == name.as_str()));
        'candidates: for name in candidates {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut variants = Vec::new();
            let mut optional = Vec::new();
            let mut nullable = Vec::new();
            for (index, (key, view)) in views.iter().enumerate() {
                let Some(property) = view.properties.get(name) else {
                    continue 'candidates;
                };
                let Some(constants) = tag_constants(resolved, property) else {
                    continue 'candidates;
                };
                if !several && constants.values.len() != 1 {
                    continue 'candidates;
                }
                // A spelling two branches share would leave the tag unable to say which it is.
                for value in &constants.values {
                    if !seen.insert(value.clone()) {
                        continue 'candidates;
                    }
                }
                if !view.required.contains(name) {
                    optional.push(index);
                }
                if constants.null {
                    nullable.push(index);
                }
                variants.push(((*key).clone(), constants.values));
            }
            // A missing or `null` tag names a branch only where it can name just one.
            let only = |positions: &[usize]| match positions {
                [position] => Some(*position),
                _ => None,
            };
            return Some(SelfNamed {
                property: name.clone(),
                variants,
                absent: only(&optional),
                null: only(&nullable),
            });
        }
    }
    None
}

/// A union whose branches name themselves, read by the constants they name themselves with.
fn self_named(named: SelfNamed) -> Shape {
    Shape::Union(Union {
        variants: named
            .variants
            .into_iter()
            .map(|(key, values)| {
                // The first spelling is the one written, and the rest are read too.
                let mut values = values.into_iter();
                Variant {
                    shape: ShapeRef::Key(key),
                    tag: values.next(),
                    aliases: values.collect(),
                }
            })
            .collect(),
        tag: Some(Tag {
            property: named.property,
            style: TagStyle::Consumed,
            absent: named.absent,
            null: named.null,
            shared: false,
        }),
    })
}

/// A union whose branches name themselves, as [`constant_discriminator`] reads it.
struct SelfNamed {
    /// The property the branches name themselves in.
    property: String,
    /// Each branch with its spellings, the one to write first.
    variants: Vec<(ShapeKey, Vec<String>)>,
    /// The position of the branch a payload without the property is.
    absent: Option<usize>,
    /// The position of the branch a payload with `null` in the property is.
    null: Option<usize>,
}

/// The union as serde's externally tagged enum, when every branch names its variant on the wire.
///
/// A branch that lists strings is one unit variant per string, and an object whose one property
/// is also its one required member is a variant named by that key, holding its value.
/// A payload is read by the key it arrives under, so the variants need no shape to tell them
/// apart, and anything else — an unlisted string or key, or an object with other members — is
/// kept whole in the open arm.
/// A last branch that accepts any other string or object, the way a forward-compatible schema
/// ends such a union, is that open arm already.
///
/// Refused where reading by key would lose or misread something: members declared beside the
/// union, a branch with something to report or any other structure, a key two branches use, or
/// a string that is also a key.
fn keyed(resolved: &ResolvedDocument, view: &View, branches: &[ShapeKey]) -> Option<Keyed> {
    // Members beside the union would have nowhere to live in the enum, and a declared
    // discriminator says the document chose a tag property instead.
    let declares_members = !view.properties.is_empty()
        || !view.required.is_empty()
        || view.additional.is_some()
        || view.additional_denied
        || view.uniform_pattern.is_some()
        || view.discriminator.is_some();
    if declares_members {
        return None;
    }
    let mut variants: Vec<KeyedVariant> = Vec::new();
    let mut units: BTreeSet<String> = BTreeSet::new();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut open = false;
    for (index, branch) in branches.iter().enumerate() {
        let view = merge::view(resolved, branch);
        // A folded branch is never classified on its own, so whatever it would have reported, or
        // a union inside it, would vanish with it.
        let reported = !view.uninterpreted.is_empty()
            || !view.unknown_types.is_empty()
            || view.conflict.is_some()
            || view.impossible
            || view.unions_collide
            || view.union.is_some();
        // A branch that also admits `null` would lose it: a keyed variant is a string or an
        // object, never `null`.
        if reported || declares_null(&view) {
            return None;
        }
        let last = index + 1 == branches.len();
        match keyed_branch(resolved, &view, branch, last)? {
            // One unit variant per string, and a string two branches list is one variant.
            KeyedBranch::Strings(listed) => {
                for value in listed {
                    if units.insert(value.clone()) {
                        variants.push(KeyedVariant {
                            name: value,
                            payload: None,
                        });
                    }
                }
            }
            // One data variant, named by its key.
            // Two branches under one key would leave the key unable to say which payload it holds.
            KeyedBranch::Member(key, payload) => {
                if !keys.insert(key.clone()) {
                    return None;
                }
                variants.push(KeyedVariant {
                    name: key,
                    payload: Some(ShapeRef::Key(payload)),
                });
            }
            // The open arm already holds whatever the catch-all accepts.
            KeyedBranch::Rest => open = true,
        }
    }
    // A string that is also a key would put two variants under one serde name, and a union with
    // no object branch is a string enum, which [`folded_strings`] already reads.
    let disjoint = units.is_disjoint(&keys);
    (disjoint && !keys.is_empty()).then_some(Keyed { variants, open })
}

/// How one branch of a keyed union names itself on the wire.
enum KeyedBranch {
    /// A string enumeration: each listed string is a unit variant.
    Strings(Vec<String>),
    /// An object whose one property is also its one required member: the key and its payload.
    Member(String, ShapeKey),
    /// The last branch, accepting any other string or object.
    Rest,
}

/// How a branch names itself on the wire, or `None` when it does not.
fn keyed_branch(
    resolved: &ResolvedDocument,
    view: &View,
    branch: &ShapeKey,
    last: bool,
) -> Option<KeyedBranch> {
    if let Some(listed) = listed_strings(resolved, view) {
        return Some(KeyedBranch::Strings(listed));
    }
    if let Some((key, payload)) = sole_member(resolved, view, branch) {
        return Some(KeyedBranch::Member(key, payload));
    }
    (last && accepts_any_string_or_object(view)).then_some(KeyedBranch::Rest)
}

/// The strings a branch lists, when it is a string enumeration and nothing else.
fn listed_strings(resolved: &ResolvedDocument, view: &View) -> Option<Vec<String>> {
    if kind_of(resolved, view) != Some(Kind::String) || !declares_no_structure(view) {
        return None;
    }
    let listed = match (&view.enumeration, &view.constant) {
        (Some(listed), _) => listed.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    let strings: Option<Vec<String>> = listed
        .iter()
        .map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();
    strings.filter(|strings| !strings.is_empty())
}

/// The key and payload of an object branch whose one property is also its one required member.
fn sole_member(
    resolved: &ResolvedDocument,
    view: &View,
    branch: &ShapeKey,
) -> Option<(String, ShapeKey)> {
    let mut properties = view.properties.iter();
    let (Some((name, payload)), None) = (properties.next(), properties.next()) else {
        return None;
    };
    // An access marker on the member is a fact about the direction it travels, which a variant
    // has nowhere to record, so a marked member stays a struct field.
    let sole = kind_of(resolved, view) == Some(Kind::Object)
        && view.required.len() == 1
        && view.required.contains(name)
        && !view.read_only.contains(name)
        && !view.write_only.contains(name)
        && view.additional.is_none()
        && view.uniform_pattern.is_none()
        && view.items.is_none()
        && view.prefix_items.is_none()
        && view.enumeration.is_none()
        && view.constant.is_none()
        && view.discriminator.is_none()
        && !names_its_subtypes(resolved, view, branch);
    sole.then(|| (name.clone(), payload.clone()))
}

/// Whether a branch accepts any string and any object, and constrains nothing else.
///
/// Only both together, which is how a forward-compatible schema ends such a union: the open arm
/// then holds exactly what the branch accepts, so [`Keyed::open`] needs to say nothing more.
fn accepts_any_string_or_object(view: &View) -> bool {
    view.types.as_ref().is_some_and(|types| {
        types.len() == 2 && types.contains(&TypeName::String) && types.contains(&TypeName::Object)
    }) && declares_no_structure(view)
        && view.enumeration.is_none()
        && view.constant.is_none()
        && view.format.is_none()
        && view.content_encoding.is_none()
        && view.content_media_type.is_none()
}

/// Whether a view declares no members, elements or other structure of its own.
fn declares_no_structure(view: &View) -> bool {
    view.properties.is_empty()
        && view.required.is_empty()
        && view.additional.is_none()
        && !view.additional_denied
        && view.uniform_pattern.is_none()
        && view.items.is_none()
        && view.prefix_items.is_none()
        && view.discriminator.is_none()
}

/// The strings a property is bound to, and whether it admits `null` too.
struct TagConstants {
    values: Vec<String>,
    null: bool,
}

/// The strings a property is bound to by `const` or `enum`, when every value but `null` is one.
///
/// Read through the nullable-emulation `anyOf: [{const: custom}, {type: null}]` too, which is how
/// a 3.1 document widens a constant with `null`.
/// `null` is a value only where every constraint on the property admits it — its own `type`, its
/// own list and the union beside them — because a payload has to satisfy all of them.
fn tag_constants(resolved: &ResolvedDocument, key: &ShapeKey) -> Option<TagConstants> {
    let view = merge::view(resolved, key);
    let own = listed_constants(&view);
    let united = view
        .union
        .as_ref()
        .map(|branches| union_constants(resolved, branches));
    match (own, united) {
        // A list of its own is every value the property can hold, and a union beside it can only
        // narrow the list further.
        (Some(own), None) => Some(own),
        (Some(own), Some(united)) => Some(TagConstants {
            null: own.null && united.is_some_and(|united| united.null),
            values: own.values,
        }),
        // Only the union lists the values, so the property's own `type` still has a say.
        (None, Some(Some(united))) => Some(TagConstants {
            null: united.null && admits_null_type(&view),
            values: united.values,
        }),
        (None, Some(None) | None) => None,
    }
}

/// The constants a union of `null` with constants lists, and whether its `null` branch is there.
///
/// Any other branch admits values no list names, so the union then lists nothing.
fn union_constants(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> Option<TagConstants> {
    let (reading, nullable) = Reading::of(resolved, branches);
    let mut constants = match reading {
        Reading::One(inner) => {
            let inner = merge::view(resolved, &inner);
            if inner.union.is_some() {
                return None;
            }
            listed_constants(&inner)?
        }
        // A tag reads one of the values it lists, so a union that admits any other string is not
        // one.
        Reading::Strings(strings) if !strings.any_other => TagConstants {
            values: strings.values,
            null: false,
        },
        Reading::Empty | Reading::Strings(_) | Reading::Variants(_) => return None,
    };
    constants.null |= nullable;
    Some(constants)
}

/// The strings a view's own `const` or `enum` lists, and whether `null` is one of its values.
fn listed_constants(view: &View) -> Option<TagConstants> {
    let declared = match (&view.enumeration, &view.constant) {
        (Some(listed), _) => listed.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    let mut values = Vec::new();
    for value in declared.iter().filter(|value| !value.is_null()) {
        values.push(value.as_str()?.to_owned());
    }
    if values.is_empty() {
        return None;
    }
    Some(TagConstants {
        values: unique(values),
        null: admits_null_type(view) && declared.iter().any(Value::is_null),
    })
}

/// Whether a view's own `type`, if it declares one, admits `null`.
fn admits_null_type(view: &View) -> bool {
    view.types
        .as_ref()
        .is_none_or(|types| types.contains(&TypeName::Null))
}

/// The discriminator every branch of this union inherits from one base schema.
///
/// A `oneOf` listing the members of a polymorphic family, where the `discriminator` is written once
/// on the family's base and each member `allOf`s that base. OpenAPI asks for the discriminator to
/// be repeated beside the `oneOf` and real documents do not: `kundenangaben` writes 37 such unions,
/// every one of which would otherwise be arbitrary JSON for want of a member the document already
/// says, one level up.
///
/// Inherited only from bases **every** branch shares. A document may write more than one — a
/// variant that belongs to two families `allOf`s both, and `kundenangaben` does exactly that for
/// its `Verwendung` types — so several are merged when they agree, and refused when they do not:
/// two bases naming one tag for two different schemas are two answers to "what is this", and
/// picking one of them would be a guess.
fn shared_base_discriminator(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
) -> Option<Discriminated> {
    let mut shared: Option<BTreeSet<SchemaId>> = None;
    for branch in branches {
        let bases: BTreeSet<SchemaId> = branch
            .parts()
            .iter()
            .map(|part| part.id())
            .filter(|&id| mapping_of(resolved, id).is_some())
            .collect();
        shared = Some(match shared {
            None => bases,
            Some(previous) => previous.intersection(&bases).copied().collect(),
        });
    }

    let mut merged: Option<Discriminated> = None;
    for base in shared? {
        let declared = mapping_of(resolved, base)?;
        let Some(into) = merged.as_mut() else {
            merged = Some(declared);
            continue;
        };
        // Two families tagging their members by different property names have no one answer.
        if into.property != declared.property {
            return None;
        }
        for (tag, target) in declared.mapping {
            match into.mapping.entry(tag) {
                btree_map::Entry::Vacant(slot) => {
                    slot.insert(target);
                }
                btree_map::Entry::Occupied(slot) => {
                    if slot.get() != &target {
                        return None;
                    }
                }
            }
        }
    }
    merged
}

/// The declared discriminator of one schema, when it names both a property and a mapping.
fn mapping_of(resolved: &ResolvedDocument, id: SchemaId) -> Option<Discriminated> {
    let Schema::Object(object) = resolved.schema(id) else {
        return None;
    };
    let declared = object.discriminator.as_ref()?;
    let mapping = declared.mapping.clone()?;
    if mapping.is_empty() {
        return None;
    }
    Some(Discriminated {
        property: declared.property_name.clone()?,
        mapping,
    })
}

/// The name a discriminated union knows one branch by.
///
/// The resolution order OpenAPI defines: an explicit `mapping` entry naming this branch wins, and
/// otherwise the branch's own component name stands in.
///
/// Both halves work on the **branch's key** rather than on the addresses its parts were written
/// at, and that is what makes them right for the inheritance spelling. A variant written as
/// `allOf: [$ref Base, {…}]` contributes nothing of its own, so its key is the base's schema plus
/// the extra part — the mapping's `#/components/schemas/BasicAuthApplication` is not among those
/// addresses, and the one address that *is* a clean component is the base's, which every sibling
/// shares. Reading a tag off that gave `okta` twenty unions whose variants were all tagged with
/// their base's name.
fn tag_value(
    resolved: &ResolvedDocument,
    declared: &Discriminated,
    component_names: &ComponentNames,
    branch: &ShapeKey,
) -> Option<String> {
    for (name, target) in &declared.mapping {
        if let Some(id) = resolved.discriminator_target(target)
            && &merge::key_of(resolved, id) == branch
        {
            return Some(name.clone());
        }
    }
    // No explicit entry: the component's own name is the implicit mapping.
    component_names.get(branch).cloned()
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
///
/// A plain string branch reads every string, and what it reads it admits, so a string meant for a
/// branch after it never reaches the branches between the two.
fn ambiguity(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> Option<String> {
    let mut strings_taken = false;
    for (index, earlier) in branches.iter().enumerate() {
        for later in branches.iter().skip(index + 1) {
            if strings_taken && reads_strings_only(resolved, later) {
                continue;
            }
            if let Some(reason) = swallows_payloads_of(resolved, earlier, later, 0) {
                return Some(reason);
            }
        }
        strings_taken |= reads_every_string(resolved, earlier);
    }
    None
}

/// Whether a branch is generated as a plain `String`, which reads every string there is.
fn reads_every_string(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    let Branch::Leaf(view) = Branch::of(resolved, key) else {
        return false;
    };
    kind_of(resolved, &view) == Some(Kind::String)
        && admitted_values(&view).is_none()
        && view.format.is_none()
        && view.content_encoding.is_none()
        && view.content_media_type.is_none()
}

/// Whether every payload a branch is meant for is a string.
///
/// A branch that also admits `null` is meant for a payload no string branch reads.
fn reads_strings_only(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    let Branch::Leaf(view) = Branch::of(resolved, key) else {
        return false;
    };
    kind_of(resolved, &view) == Some(Kind::String) && !declares_null(&view)
}

/// How far into nested arrays and unions the ambiguity test follows a branch before giving up.
///
/// A union of arrays is ambiguous exactly when its element types are, so the test recurses — but a
/// document can nest arbitrarily and a recursive schema can nest forever, so the depth is capped
/// and the cap answers "ambiguous", which is the direction that degrades rather than guesses.
const MAX_AMBIGUITY_DEPTH: usize = 4;

/// Why the branch tried first would accept a payload meant for the later one, if it would.
///
/// A branch is compared as the type generated for it, which for a nested union is not the
/// union's branches.
/// The type generated for a union of two or more branches is open: it holds every payload none of
/// its own branches fits, so as the earlier branch it swallows every later one.
/// As the later branch it is only as hard to reach as its own branches, and how it tells those
/// apart is settled where it is classified itself, so they are not compared with one another here.
fn swallows_payloads_of(
    resolved: &ResolvedDocument,
    earlier: &ShapeKey,
    later: &ShapeKey,
    depth: usize,
) -> Option<String> {
    // One branch reached twice, directly and through a nested union, reads a payload into the same
    // value either way, so whichever matches is the right answer.
    if earlier == later {
        return None;
    }
    if depth > MAX_AMBIGUITY_DEPTH {
        return Some("its branches nest deeper than the shape comparison follows".to_owned());
    }
    let depth = depth.saturating_add(1);
    if lists_beside_its_union(resolved, earlier) {
        return Some(
            "one of its branches lists objects or lists beside a union of its own, and the type \
             generated for that union reads values the list leaves out, so it may accept the \
             payloads of a later branch"
                .to_owned(),
        );
    }
    let (first, first_flattened) = match Branch::of(resolved, earlier) {
        Branch::Through(inner) => return swallows_payloads_of(resolved, &inner, later, depth),
        Branch::Union(_) => {
            return Some(
                "one of its branches is itself a union and is not the last, and the type \
                 generated for it holds every payload its own branches do not fit, so it accepts \
                 the payloads meant for every branch after it"
                    .to_owned(),
            );
        }
        Branch::Leaf(view) => (view, false),
        Branch::Flattened(view) => (view, true),
    };
    let (second, flattened) = match Branch::of(resolved, later) {
        Branch::Through(inner) => return swallows_payloads_of(resolved, earlier, &inner, depth),
        Branch::Union(branches) => {
            return branches
                .iter()
                .find_map(|branch| swallows_payloads_of(resolved, earlier, branch, depth));
        }
        Branch::Leaf(view) => (view, false),
        Branch::Flattened(view) => (view, true),
    };

    // A branch that constrains nothing accepts every payload its siblings do — but an untagged
    // enum tries its variants in declaration order, so one sitting *last* is a catch-all that only
    // ever runs when everything else has failed.
    // That is a shape real documents write on purpose ("this list of things, or an untyped
    // list"), and behind branches that admit what they read it loses nothing.
    // Anywhere else the same branch swallows the ones after it, which loses everything.
    let Some(left) = kind_of(resolved, &first) else {
        return Some(
            "one of its branches says nothing about the shape of its own payload and is not the \
             last, so it accepts the payloads meant for every branch after it"
                .to_owned(),
        );
    };
    // A later branch that admits every payload of the earlier one's kind is a catch-all, reached
    // only by what every branch before it refuses, and it declares no members an earlier branch
    // could drop.
    // An earlier branch whose type accepts a payload its own schema refuses takes one of the
    // catch-all's, such as an unlisted value of an enum it lists.
    let right = kind_of(resolved, &second);
    if right.is_none_or(|right| right == left && !flattened && admits_its_kind(resolved, &second)) {
        let overreaches = first_flattened || view_overreaches(resolved, &first, 0);
        return overreaches.then(|| {
            "one of its branches reads values its schema does not admit, such as an unlisted \
             value of an enum, so it accepts payloads meant for a later branch that admits every \
             value of its kind"
                .to_owned()
        });
    }
    if right != Some(left) {
        return None;
    }
    match left {
        // A closed earlier branch that keeps every part of what it reads loses nothing by taking a
        // payload a later one could also read, so first match is a faithful reading.
        // That holds only for payloads it admits: one with a member's unlisted value is a later
        // branch's, however whole the earlier one keeps it.
        // A base its discriminator's mapping makes a union is not the struct its view describes.
        Kind::Object
            if !names_its_subtypes(resolved, &first, earlier)
                && reads_whole(resolved, &first, 0)
                && !view_overreaches(resolved, &first, 0) =>
        {
            None
        }
        // The members a later flattened union may carry are not in its view, so an earlier object
        // is never shown to refuse its payloads.
        Kind::Object if flattened => Some(
            "one of its branches is an object, and a later one has a union beside its members \
             whose branches may carry the members the earlier one requires, so the earlier one \
             may accept its payloads"
                .to_owned(),
        ),
        Kind::Object => (!rejects(resolved, &first, &second, 0)).then(|| {
            "one of its branches is an object that accepts the payloads of a later one, which is \
             tried after it and so never reached"
                .to_owned()
        }),
        // Two lists are told apart by what is in them, which is the same question one level down.
        Kind::Array => {
            let (Some(left), Some(right)) = (&first.items, &second.items) else {
                return Some(
                    "two of its branches are lists and at least one does not say what is in it"
                        .to_owned(),
                );
            };
            if left == right {
                return None;
            }
            swallows_payloads_of(resolved, left, right, depth)
        }
        Kind::String | Kind::Number | Kind::Boolean => scalar_swallows(&first, &second),
        Kind::Null => None,
    }
}

/// Why an earlier scalar branch would take a later one's payloads, if it would.
///
/// Two branches of one scalar kind read the same bytes into the same Rust type, so whichever
/// matches first is a faithful reading and nothing is lost.
/// That stops holding once the earlier branch lists its values.
/// No type generated for a list enforces it: a string enum is open, and a numeric or boolean one
/// is the plain scalar.
/// So the earlier branch takes every value of its kind, including the ones only a later branch
/// admits.
/// A later value the earlier type cannot hold at all, such as `2.5` against an integer, still
/// reaches the later branch.
fn scalar_swallows(first: &View, second: &View) -> Option<String> {
    let listed = admitted_values(first)?;
    let reached = |later: Vec<Value>| {
        later
            .iter()
            .all(|value| listed.contains(&value.to_string()) || integer_refuses(first, value))
    };
    (!listed_values(second).is_some_and(reached)).then(|| {
        "one of its branches lists the values it admits, but the type generated for it reads any \
         value of its kind, so it accepts the payloads of a later one, which is tried after it and \
         so never reached"
            .to_owned()
    })
}

/// One branch of a union, as the type generated for it reads a payload.
enum Branch {
    /// A union of one branch and perhaps `null`, which reads exactly what that branch does.
    Through(ShapeKey),
    /// A union of two or more branches, which [`swallows_payloads_of`] compares by its branches.
    Union(Vec<ShapeKey>),
    /// Anything else, including a union of strings, which is one string enum.
    Leaf(Box<View>),
    /// A struct with a union flattened beside its members, as the view of those members alone.
    ///
    /// As the earlier branch it reads a payload as the open struct of its members does, because
    /// the union's fallback keeps whatever its branches do not.
    /// As the later branch its payloads may also carry any member its union's branches declare,
    /// which its view does not list, so no earlier object is shown to refuse them.
    Flattened(Box<View>),
}

impl Branch {
    fn of(resolved: &ResolvedDocument, key: &ShapeKey) -> Self {
        let mut view = merge::view(resolved, key);
        // A base its discriminator's mapping makes a union is that union, whose fallback holds
        // whatever none of its subtypes fits, however much its view looks like one object.
        let subtypes = subtypes(resolved, &view, key).branches;
        if subtypes.len() >= 2 {
            return Self::Union(subtypes);
        }
        if view
            .union
            .as_deref()
            .is_some_and(|branches| flattens(resolved, &view, branches))
        {
            view.union = None;
            return Self::Flattened(Box::new(view));
        }
        // [`key`] gives up on these before it reads their branches, and [`kind_of`] reads them as
        // the arbitrary JSON they become.
        let degraded = view.impossible || view.conflict.is_some() || view.unions_collide;
        let Some(branches) = view.union.as_deref().filter(|_| !degraded) else {
            return Self::Leaf(Box::new(view));
        };
        let (reading, nullable) = Reading::of(resolved, branches);
        match reading {
            Reading::One(key) => Self::Through(key),
            // An enum that admits any other string reads every string, as a bare string does.
            // A `null` among them makes the enum optional, which reads `null` as well.
            Reading::Strings(strings) => Self::Leaf(Box::new(View {
                types: Some(if nullable {
                    BTreeSet::from([TypeName::String, TypeName::Null])
                } else {
                    BTreeSet::from([TypeName::String])
                }),
                enumeration: (!strings.any_other)
                    .then(|| strings.values.into_iter().map(Value::String).collect()),
                ..View::default()
            })),
            Reading::Variants(branches) => Self::Union(branches),
            // What [`union`] makes of it: `null`, or arbitrary JSON when there is not even that.
            Reading::Empty if nullable => Self::Leaf(Box::new(View {
                types: Some(BTreeSet::from([TypeName::Null])),
                ..View::default()
            })),
            Reading::Empty => Self::Leaf(Box::default()),
        }
    }
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
fn kind_of(resolved: &ResolvedDocument, view: &View) -> Option<Kind> {
    // Members beside a union no branch names are a struct, whatever the union holds.
    if view
        .union
        .as_deref()
        .is_some_and(|branches| flattens(resolved, view, branches))
    {
        return Some(Kind::Object);
    }
    // The open tuple [`array`] gives up on is arbitrary JSON, for the reason the degraded views
    // in [`declared_kind`] are.
    declared_kind(view).filter(|&kind| kind != Kind::Array || !open_tuple(resolved, view))
}

fn declared_kind(view: &View) -> Option<Kind> {
    if view.union.is_some() {
        // A branch that is itself a union has no single kind, and unfolding it here would be a
        // second, quieter copy of this whole function.
        return None;
    }
    // What [`key`] answers before it ever reaches [`core`]. These branches become
    // `serde_json::Value`, which accepts every payload each of its siblings does, so reporting the
    // kind their remaining keywords suggest would claim a branch discriminates when the type
    // actually emitted for it cannot. The question this function answers is about the emitted type,
    // not about the schema.
    if view.impossible || view.conflict.is_some() || view.unions_collide {
        return None;
    }
    // `type: file` is a string by the time the type is chosen, which [`report_unknown_types`]
    // repairs on a view of its own.
    let mut types = view.types.clone();
    if view.unknown_types.iter().any(|spelling| spelling == "file") {
        types
            .get_or_insert_with(BTreeSet::new)
            .insert(TypeName::String);
    }
    // The precedence [`nullable`] and [`core`] give the emitted type: `type: null` alone, then a
    // list of strings, which is a string enum whatever `type` says, then `type`, then the keywords
    // in [`untyped`]'s order.
    if types
        .as_ref()
        .is_some_and(|types| types.len() == 1 && types.contains(&TypeName::Null))
    {
        return Some(Kind::Null);
    }
    if enumerated_kind(view) == Some(Kind::String) {
        return Some(Kind::String);
    }
    let named: Vec<&TypeName> = types
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
    if !view.properties.is_empty() || view.additional.is_some() || view.additional_denied {
        return Some(Kind::Object);
    }
    if view.items.is_some() || view.prefix_items.is_some() {
        return Some(Kind::Array);
    }
    if view.uniform_pattern.is_some() {
        return Some(Kind::Object);
    }
    match enumerated_kind(view) {
        // A bare enumeration names its kind as plainly as `type` would, and these two have a Rust
        // scalar of their own to be read into.
        Some(kind @ (Kind::Number | Kind::Boolean)) => return Some(kind),
        // An enumeration of objects or lists narrows nothing progeny can express, so the branch
        // becomes `serde_json::Value` and discriminates nothing.
        Some(Kind::String | Kind::Object | Kind::Array | Kind::Null) | None => {}
    }
    if view.format.is_some() || view.content_encoding.is_some() || view.content_media_type.is_some()
    {
        return Some(Kind::String);
    }
    None
}

/// The one JSON kind every value of an `enum`/`const` has, when they agree on one.
///
/// `null` is nullability rather than a value and the caller has already read it as such. Values of
/// mixed kinds have no single Rust type at all, which is the case [`string_values`] reports.
fn enumerated_kind(view: &View) -> Option<Kind> {
    let declared = match (&view.enumeration, &view.constant) {
        (Some(values), _) => values.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    let mut kinds = declared
        .iter()
        .filter(|value| !value.is_null())
        .map(|value| match value {
            Value::String(_) => Kind::String,
            Value::Number(_) => Kind::Number,
            Value::Bool(_) => Kind::Boolean,
            Value::Array(_) => Kind::Array,
            Value::Object(_) | Value::Null => Kind::Object,
        });
    let first = kinds.next()?;
    kinds.all(|kind| kind == first).then_some(first)
}

/// Whether the branch tried *first* is guaranteed to turn down the payloads of a later one.
///
/// The order is the question, and asking it symmetrically is wrong in a way that produces silently
/// wrong output. serde tries variants in declaration order and takes the first that deserializes,
/// so what has to hold for a pair is not that *something* tells them apart but that the one tried
/// first refuses the other's payloads. A required property the later branch never declares does
/// that. The same property required by the *later* branch does not: by the time it would have said
/// so, the earlier branch has already accepted the payload and dropped the member that would have
/// distinguished it — a `{name, id}` read as the `{name}` branch, losing `id` with nothing
/// reported.
///
/// Three ways an earlier branch refuses, all of which serde's untagged matching acts on:
///
/// * it requires a property the later branch never declares, so no payload of the later one carries
///   it — when it declares the property too, and not as nullable, since otherwise its struct reads
///   the missing member as `None`;
/// * it refuses members it does not declare, and every payload of the later one carries one;
/// * a property both declare, and the later branch requires and never sets to `null`, holds a
///   different *kind* of value in each — `workos` returns
///   `{message: string}` or `{message: [string], error: string}` from one status — because
///   deserializing an array into the earlier branch's `String` is an error, and an error is a
///   fall-through to the next variant. This one is easy to forget because the property has the same
///   name in both, which is what makes the branches *look* alike.
///
/// A property both declare with disjoint sets of values is not a fourth way.
/// The type generated for the property reads any value of its kind, so the earlier branch accepts
/// the later one's constant and reads it as an unlisted value of its own.
/// The `const`-tagged union that rule looks made for is read by its tag instead, in
/// [`constant_discriminator`] or through its declared discriminator.
///
/// A payload the earlier branch accepts *and* the later branch validates is ambiguous in the
/// document itself, not in this reading of it, so it is not what these rules are for.
///
/// A later branch's payloads are the ones its declared members describe.
/// A member it never declares is taken to be absent, although an open object may carry it.
/// So `{"id": "b", "x": "keep"}` against `[{id: enum[a]}, {x}]` reads as the first branch, with
/// `id` an unlisted value of its open enum, and `x` is dropped as any undeclared member is.
/// That payload carries the first branch's whole shape, and reading it so is the forward
/// compatibility the open enum is for.
/// Judging an open later branch by every payload it validates instead degrades about 200 unions
/// across the corpus, mostly ones whose branches only their required members tell apart.
/// A later branch that declares no members is a catch-all, which [`swallows_payloads_of`] judges
/// by what the earlier branch's own schema admits instead.
fn rejects(resolved: &ResolvedDocument, earlier: &View, later: &View, depth: usize) -> bool {
    // A member every payload of `view` carries and the generated struct insists on.
    // A required member that may be `null` is an `Option`, which reads an absent member as `None`,
    // and a required name with no declared property has no field at all.
    let insists = |view: &View, name: &str| {
        view.required.contains(name)
            && view
                .properties
                .get(name)
                .is_some_and(|key| !generates_option(resolved, key, 0))
    };
    // A member the later branch does not declare is absent from its payloads only where nothing
    // else holds members: a map or a typed catch-all may carry any name.
    let lists_its_members = later.additional.is_none()
        && later.uniform_pattern.is_none()
        && (!later.properties.is_empty() || later.additional_denied);
    let kind = |key: &ShapeKey| member_kind(resolved, &merge::view(resolved, key), key);
    let missing = |name: &String| {
        if later.properties.contains_key(name) {
            return false;
        }
        if lists_its_members {
            return true;
        }
        // A catch-all gives the member its own kind, and a pattern may not match the name at all.
        let catch_all = later
            .additional
            .as_ref()
            .filter(|_| later.uniform_pattern.is_none());
        catch_all
            .zip(earlier.properties.get(name))
            .is_some_and(|(value, own)| match (kind(own), kind(value)) {
                (Some(own), Some(value)) => own != value,
                _ => false,
            })
    };
    if earlier
        .required
        .iter()
        .any(|name| insists(earlier, name) && missing(name))
    {
        return true;
    }
    // Only sound in this direction: a closed struct is the one shape that rejects a payload for
    // carrying something *extra*, so what the later branch insists on is what turns it down.
    if earlier.additional_denied
        && later
            .required
            .iter()
            .any(|name| !earlier.properties.contains_key(name))
    {
        return true;
    }
    earlier.properties.iter().any(|(name, key)| {
        let Some(other) = later.properties.get(name) else {
            return false;
        };
        // Either every later payload shows the earlier struct the other kind, or the earlier
        // struct insists on a value of its own kind and so turns down an absent or `null` one too.
        // Without either, a later payload that leaves the member out, or sets it to `null`, never
        // shows the earlier struct the other kind.
        if !insists(later, name) && !insists(earlier, name) {
            return false;
        }
        let (here_view, there_view) = (merge::view(resolved, key), merge::view(resolved, other));
        let (Some(here), Some(there)) = (
            member_kind(resolved, &here_view, key),
            member_kind(resolved, &there_view, other),
        ) else {
            return false;
        };
        if here != there {
            return true;
        }
        // Two objects are told apart by their own members, the same question one level down.
        // A member with a declared union may carry members its view does not list, so it is not
        // shown to refuse anything.
        here == Kind::Object
            && depth < MAX_AMBIGUITY_DEPTH
            && here_view.union.is_none()
            && there_view.union.is_none()
            && rejects(resolved, &here_view, &there_view, depth.saturating_add(1))
    })
}

/// The kind of the type generated for a member, by [`kind_of`], or none for a base its
/// discriminator's mapping makes a union.
///
/// That union holds whatever none of its subtypes fits, so its payloads have no one kind, though
/// its view describes an object.
fn member_kind(resolved: &ResolvedDocument, view: &View, key: &ShapeKey) -> Option<Kind> {
    if names_its_subtypes(resolved, view, key) {
        return None;
    }
    kind_of(resolved, view)
}

/// Whether the type generated for a view keeps every part of each payload it accepts.
///
/// An earlier branch like that loses nothing by reading a payload a later one could also read:
/// what it accepts it writes back whole, and what it cannot hold whole it refuses.
/// That takes a closed struct, because an open one reads past what it does not declare, and
/// members that keep everything too, as far as [`MAX_AMBIGUITY_DEPTH`]:
///
/// - a boolean, an integer, or a string with no `format` or content keywords to re-spell it;
/// - a list or map of such values;
/// - a closed struct of them whose every member is required and not `null`, because an `Option`
///   reads an explicit `null` and an absent member alike, and writes back only one of them;
/// - arbitrary JSON, which keeps anything.
///
/// A `number` is an `f64`, which rounds an integer too large for it.
/// A union keeps what its branches keep, which is a question this does not follow, so it answers
/// no, and so does a schema whose discriminator's mapping makes it one.
fn reads_whole(resolved: &ResolvedDocument, view: &View, depth: usize) -> bool {
    if depth > MAX_AMBIGUITY_DEPTH || view.union.is_some() {
        return false;
    }
    let member = |key: &ShapeKey| {
        let view = merge::view(resolved, key);
        !names_its_subtypes(resolved, &view, key) && reads_whole(resolved, &view, depth + 1)
    };
    match kind_of(resolved, view) {
        // Nothing decides a kind, so the type is arbitrary JSON.
        None => !(view.impossible || view.conflict.is_some() || view.unions_collide),
        Some(Kind::String) => {
            view.format.is_none()
                && view.content_encoding.is_none()
                && view.content_media_type.is_none()
        }
        Some(Kind::Number) => view.types.as_ref().is_some_and(|types| {
            types.contains(&TypeName::Integer) && !types.contains(&TypeName::Number)
        }),
        Some(Kind::Boolean | Kind::Null) => true,
        Some(Kind::Array) => view.prefix_items.is_none() && view.items.as_ref().is_none_or(member),
        // A map keeps every member, as whole as its values are.
        Some(Kind::Object) if view.properties.is_empty() && !view.additional_denied => view
            .additional
            .as_ref()
            .or(view.uniform_pattern.as_ref())
            .is_none_or(member),
        Some(Kind::Object) => {
            view.additional_denied
                && view.properties.iter().all(|(name, key)| {
                    view.required.contains(name)
                        && !generates_option(resolved, key, 0)
                        && member(key)
                })
        }
    }
}

/// Whether the type generated for a branch accepts a payload its own schema refuses.
///
/// A payload like that is not the branch's, so when a later branch admits it, the later one is
/// the branch it was meant for, and taking it first reads it as the wrong variant.
/// The over-acceptance that decides this under progeny's types is a listed value: a string enum
/// keeps an unlisted string as its `Unknown`, and a numeric or boolean list is the plain scalar.
/// So does a union, which holds what none of its branches fits in its fallback, a branch
/// generated as arbitrary JSON because progeny cannot express its schema, and a required member
/// whose field is an `Option`, which reads its absence as `None`.
///
/// Validation keywords such as `pattern` or `minimum` are never enforced, so they do not count.
/// Neither does an optional member reading an explicit `null` as absent: a payload that sends
/// `null` for a member it may leave out is read as the branch that declares the member, which is
/// the reading every struct gives it.
/// Every declared member counts, required or not, and so do a list's items and a map's values,
/// as far as [`MAX_AMBIGUITY_DEPTH`]; a branch deeper than that answers yes.
fn overreaches(resolved: &ResolvedDocument, key: &ShapeKey, depth: usize) -> bool {
    if depth > MAX_AMBIGUITY_DEPTH {
        return true;
    }
    if lists_beside_its_union(resolved, key) {
        return true;
    }
    match Branch::of(resolved, key) {
        Branch::Through(inner) => overreaches(resolved, &inner, depth.saturating_add(1)),
        Branch::Union(_) | Branch::Flattened(_) => true,
        Branch::Leaf(view) => view_overreaches(resolved, &view, depth),
    }
}

/// [`overreaches`] for a branch already read as one view, which is neither a union nor a struct
/// with a union flattened beside its members.
fn view_overreaches(resolved: &ResolvedDocument, view: &View, depth: usize) -> bool {
    // What [`key`] gives up on is arbitrary JSON, which accepts what the schema meant to refuse.
    if view.impossible || view.conflict.is_some() || view.unions_collide {
        return true;
    }
    // A list the generated type does not hold, which [`merge::view`] reports, admits more than
    // it lists.
    if view
        .uninterpreted
        .iter()
        .any(|keyword| matches!(*keyword, "enum" | "const"))
    {
        return true;
    }
    let member = |key: &ShapeKey| overreaches(resolved, key, depth.saturating_add(1));
    match kind_of(resolved, view) {
        // A list of `null` alone is the unit type, which reads nothing else.
        Some(Kind::Null) => false,
        // A list of both booleans admits every boolean, unless a constant beside it admits one.
        // A `null` in the `type` or the list makes the field an `Option`, which reads `null`
        // although the schema admits it only where both do.
        Some(Kind::Boolean)
            if view.constant.is_none()
                && admitted_values(view).is_some_and(|values| {
                    let typed_null = view
                        .types
                        .as_ref()
                        .is_none_or(|types| types.contains(&TypeName::Null));
                    values.contains("true")
                        && values.contains("false")
                        && (!declares_null(view) || (typed_null && values.contains("null")))
                }) =>
        {
            false
        }
        Some(_) if admitted_values(view).is_some() => true,
        Some(Kind::String | Kind::Number | Kind::Boolean) => false,
        // A tuple whose tail admits nothing reads its prefix alone and refuses a longer list.
        // With no prefix there is no tuple, and the list reads its items as they are generated.
        Some(Kind::Array) => view
            .items
            .iter()
            .filter(|key| {
                view.prefix_items.as_ref().is_none_or(Vec::is_empty)
                    || !merge::view(resolved, key).impossible
            })
            .chain(view.prefix_items.iter().flatten())
            .any(member),
        // A required name is enforced only by a field that is not an `Option`, which reads an
        // absent member as `None`.
        Some(Kind::Object) => {
            view.required.iter().any(|name| {
                view.properties
                    .get(name)
                    .is_none_or(|key| generates_option(resolved, key, 0))
            }) || view
                .properties
                .values()
                .chain(&view.additional)
                .chain(&view.uniform_pattern)
                .any(member)
        }
        // An open tuple, which [`kind_of`] reads as arbitrary JSON although it describes a list.
        None if declared_kind(view).is_some() => true,
        // A `type` that names several kinds is arbitrary JSON, which reads the kinds it does not
        // name too, and so are required names with nothing to say they belong to an object.
        // A branch that constrains nothing refuses nothing, and neither does its type.
        None => {
            admitted_values(view).is_some()
                || !view.required.is_empty()
                || view
                    .types
                    .iter()
                    .flatten()
                    .any(|name| *name != TypeName::Null)
        }
    }
}

/// Whether a schema lists values beside a union of its own that the merged view could not narrow
/// it to.
///
/// [`merge::view`] reads a list of strings, numbers or booleans beside a union as that list, so
/// only a list holding an object or a list keeps its union here.
/// That union's type stands in for the schema, and it reads values the list leaves out.
fn lists_beside_its_union(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    let view = merge::view(resolved, key);
    view.union.is_some() && admitted_values(&view).is_some()
}

/// Whether a branch admits every value of its kind, as an object or a list that says nothing
/// about its members, and so is generated as a map or list of arbitrary JSON.
fn admits_its_kind(resolved: &ResolvedDocument, view: &View) -> bool {
    // A member, item or value schema that says nothing about its payloads, and is no union.
    let unconstrained = |key: &ShapeKey| match Branch::of(resolved, key) {
        Branch::Leaf(view) => {
            kind_of(resolved, &view).is_none() && admitted_values(&view).is_none()
        }
        Branch::Through(_) | Branch::Union(_) | Branch::Flattened(_) => false,
    };
    if admitted_values(view).is_some() {
        return false;
    }
    match kind_of(resolved, view) {
        Some(Kind::Object) => {
            view.properties.is_empty()
                && view.required.is_empty()
                && !view.additional_denied
                && view.uniform_pattern.is_none()
                && view.additional.as_ref().is_none_or(unconstrained)
        }
        Some(Kind::Array) => {
            view.prefix_items.is_none() && view.items.as_ref().is_none_or(unconstrained)
        }
        _ => false,
    }
}

/// Whether the type generated for a schema is an `Option`, which reads an absent member as `None`.
///
/// Followed through a union of one branch, which is that branch's type, as far as
/// [`MAX_AMBIGUITY_DEPTH`]; an alias chain longer than that answers yes, which is the direction
/// that proves no separation.
/// A `null` in a nested union [`spread_union`] may read as a branch of this one counts, whether
/// or not the union is spread, for the same reason.
fn generates_option(resolved: &ResolvedDocument, key: &ShapeKey, depth: usize) -> bool {
    if depth > MAX_AMBIGUITY_DEPTH {
        return true;
    }
    let view = merge::view(resolved, key);
    if declares_null(&view) {
        return true;
    }
    let Some(branches) = view.union.as_deref() else {
        return false;
    };
    let mut spread = Vec::new();
    spread_into(resolved, branches, &mut BTreeSet::new(), &mut spread);
    match Reading::of(resolved, &spread) {
        (_, true) => true,
        (Reading::One(inner), false) => generates_option(resolved, &inner, depth.saturating_add(1)),
        (Reading::Empty | Reading::Strings(_) | Reading::Variants(_), false) => false,
    }
}

/// The finite set of values a branch admits, when it names one.
///
/// Only `const` and `enum` bound a branch to a finite set; everything else admits values no
/// comparison can enumerate, and saying so is what keeps the subset test from claiming a branch
/// is covered when it is not.
fn admitted_values(view: &View) -> Option<BTreeSet<String>> {
    Some(
        listed_values(view)?
            .iter()
            .map(ToString::to_string)
            .collect(),
    )
}

/// The values a branch's `enum` or `const` lists, as the document spells them.
fn listed_values(view: &View) -> Option<Vec<Value>> {
    match (&view.enumeration, &view.constant) {
        (Some(listed), _) => Some(listed.clone()),
        (None, Some(value)) => Some(vec![value.clone()]),
        (None, None) => None,
    }
}

/// Whether the type generated for a branch is an integer that cannot hold `value`.
///
/// A branch is an integer where its `type` says so, or where it names no type and lists whole
/// numbers alone, the way [`core`] reads it.
/// The integer refuses a number with a fractional part, and an unsigned one also refuses a
/// negative number.
/// A whole number spelled `2.0` is not refused, because the schema admits `2` by the same value,
/// and the integer reads that.
/// A whole number too large for the integer is not counted, which is the direction that degrades.
fn integer_refuses(view: &View, value: &Value) -> bool {
    // The type is the wide one or the one the format names, as the configuration says, and the
    // value is refused only where both refuse it.
    // A value whose integer reading cannot be told is not shown refused either.
    if let Ok(Some(width)) = format_width(view)
        && let Value::Number(number) = value
    {
        let (least, greatest) = width.range();
        match merge::rounded(number) {
            None => return false,
            Some((floor, ceil)) if floor == ceil && (least..=greatest).contains(&floor) => {
                return false;
            }
            Some(_) => {}
        }
    }
    let named: Vec<&TypeName> = view
        .types
        .iter()
        .flatten()
        .filter(|name| **name != TypeName::Null)
        .collect();
    let integer = match named.as_slice() {
        [TypeName::Integer] => true,
        [] => integral(view),
        _ => false,
    };
    let fractional = value.as_f64().is_some_and(|number| number.fract() != 0.0);
    let negative = value.as_f64().is_some_and(|number| number < 0.0);
    integer && value.is_number() && (fractional || (view.unsigned && negative))
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
        let names = values
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned));
        return Some(unique(names));
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

/// The strings a union's branches name, when every branch lists only strings or reads any string.
///
/// A generated string enum is open, so one enum of every listed value reads each string the way
/// the document means it.
/// Kept as separate variants instead, each branch's enum would read every string, the first would
/// take them all, and the branches after it would never be reached.
///
/// A branch that reads any string, the `{type: string}` beside `{enum: [small, large]}` a document
/// writes for "known values, or any other", names no value of its own.
/// It makes every other string one the document allows, whichever order the branches come in.
/// At least one branch has to list a value, or there is no enum.
/// The second half of the answer says whether a branch admits `null` too, by listing it or as a
/// catch-all whose type includes it, which the enum then has to keep.
///
/// A branch with something to report, or a union of its own, is not folded in: it is reported and
/// read where it is classified, and folded into the enum it would never be classified at all.
fn folded_strings(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
) -> Option<(StringEnum, bool)> {
    let mut values = Vec::new();
    let mut any_other = false;
    let mut null = false;
    for branch in branches {
        let view = merge::view(resolved, branch);
        let reported = !view.uninterpreted.is_empty()
            || !view.unknown_types.is_empty()
            || view.conflict.is_some()
            || view.impossible
            || view.unions_collide
            || view.union.is_some();
        if reported {
            return None;
        }
        let declared = match (&view.enumeration, &view.constant) {
            (Some(listed), _) => listed.clone(),
            (None, Some(value)) => vec![value.clone()],
            (None, None) if reads_any_string(resolved, &view) => {
                any_other = true;
                null |= declares_null(&view);
                continue;
            }
            (None, None) => return None,
        };
        for value in declared {
            // A listed `null`, as a 3.0 `nullable` enum lists it, makes the enum optional, where
            // the branch's own `type` admits it: `{type: string, enum: [a, null]}` does not.
            if value.is_null() {
                null |= admits_null_type(&view);
                continue;
            }
            values.push(value.as_str()?.to_owned());
        }
    }
    (!values.is_empty()).then(|| {
        let strings = StringEnum {
            values: unique(values),
            any_other,
        };
        (strings, null)
    })
}

/// Whether a branch is a string and nothing more: no listed values, structure, format or encoding.
fn reads_any_string(resolved: &ResolvedDocument, view: &View) -> bool {
    kind_of(resolved, view) == Some(Kind::String)
        && declares_no_structure(view)
        && view.format.is_none()
        && view.content_encoding.is_none()
        && view.content_media_type.is_none()
}

/// The listed values with repeats dropped, in the order the document wrote them.
///
/// JSON Schema calls `enum` a set, and documents still repeat values in it. Two variants renamed to
/// one string would give serde a variant it can never produce: the second is unreachable coming in
/// and indistinguishable going out, so it round-trips as the first. Dropping the repeat loses
/// nothing, because a set with a member written twice is the same set.
///
/// Repeats are not adjacent in practice — `["a", "b", "a"]` is how a document writes one — so
/// [`Vec::dedup`], which only collapses runs, does not do this.
fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
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

/// Report the keywords the shape does not express.
///
/// It runs once the shape is chosen, because only the chosen type says what is expressed.
/// An unsigned integer holds every integer from zero up, so a lower bound that admits zero and
/// nothing below it narrows nothing.
fn report_uninterpreted(view: &View, shape: &Shape, ctx: &mut Ctx, at: &JsonPointer) {
    // A bound the integer's own range ends at is what the type expresses: a zero `minimum` on an
    // unsigned integer, or a `maximum` of 65535 on a `u16`.
    let integer = match shape {
        Shape::Scalar(Scalar::Integer(integer)) => Some(*integer),
        Shape::Optional(ShapeRef::Inline(inner)) => match **inner {
            Shape::Scalar(Scalar::Integer(integer)) => Some(integer),
            _ => None,
        },
        _ => None,
    };
    let (least, greatest) = integer.map_or((None, None), |integer| {
        let (least, greatest) = integer.range();
        (Some(least), Some(greatest))
    });
    let expressed = |bound: Option<merge::Bound>, end: Option<i128>| match (bound, end) {
        (Some(merge::Bound::At(bound)), Some(end)) => bound == end,
        _ => false,
    };
    let lower_expressed = expressed(view.lower, least);
    let upper_expressed = expressed(view.upper, greatest);
    let keywords: Vec<String> = view
        .uninterpreted
        .iter()
        .filter(|&&keyword| match keyword {
            "minimum" | "exclusiveMinimum" => !lower_expressed,
            "maximum" | "exclusiveMaximum" => !upper_expressed,
            _ => true,
        })
        .map(|keyword| format!("`{keyword}`"))
        .collect();
    if keywords.is_empty() {
        return;
    }
    let verb = if keywords.len() == 1 {
        "narrows"
    } else {
        "narrow"
    };
    degrade(
        ctx,
        at,
        BreakageClass::UnsupportedConstruct,
        &format!(
            "{} {verb} which values are valid in a way no Rust type expresses; the generated type \
             accepts more than the document allows",
            keywords.join(", "),
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
