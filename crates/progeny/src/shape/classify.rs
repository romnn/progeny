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
    Docs, Extra, Field, Format, MAX_FIXED_ARRAY, Scalar, Shape, ShapeKey, ShapeRef, Struct, Tag,
    TagStyle, Union, Variant,
};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::{Schema, SchemaId, TypeName};

/// The name `components.schemas` gave each shape, which is what a discriminator falls back to
/// when its mapping does not name a branch.
pub(super) type ComponentNames = BTreeMap<ShapeKey, String>;

/// Classify one key.
pub(crate) fn key(
    resolved: &ResolvedDocument,
    key: &ShapeKey,
    component_names: &ComponentNames,
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
    report_uninterpreted(&view, ctx, &at);
    inheritance_union(resolved, &mut view, key, ctx, &at);

    nullable(resolved, &view, component_names, ctx, &at)
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
    if view.union.is_some() {
        return;
    }
    let Some(declared) = &view.discriminator else {
        return;
    };
    if declared.mapping.is_empty() || key.parts().len() != 1 {
        return;
    }

    let mut branches: Vec<ShapeKey> = Vec::new();
    let mut unmapped: Vec<&str> = Vec::new();
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
    for target in unmapped {
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

    let core = core(resolved, view, others.as_ref(), component_names, ctx, at);
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
    types: Option<&BTreeSet<TypeName>>,
    component_names: &ComponentNames,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    if let Some(branches) = &view.union {
        return union(resolved, view, branches, component_names, ctx, at);
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
        Some(TypeName::Array) => array(resolved, view, ctx, at),
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
            Scalar::Integer {
                signed: !view.unsigned,
            }
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
    Shape::Struct(Struct { fields, extra })
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
    let (reading, nullable) = Reading::of(resolved, branches);
    let inner = match reading {
        Reading::Empty => {
            return if nullable { Shape::Null } else { Shape::Any };
        }
        // `anyOf: [T, {type: null}]` — the nullable-emulation pattern, and the reason to look at
        // the branches at all before treating this as a union.
        Reading::One(key) => Shape::Alias(ShapeRef::Key(key)),
        Reading::Strings(values) => Shape::StringEnum(values),
        Reading::Variants(rest) => {
            if let Some((property, tags)) = constant_discriminator(resolved, &rest) {
                // The branches name themselves: read the tag, the way a declared discriminator
                // is read, rather than match by shape — which would rest on the constants, and a
                // generated string enum reads any string.
                Shape::Union(Union {
                    variants: tags
                        .into_iter()
                        .map(|(key, tag)| Variant {
                            shape: ShapeRef::Key(key),
                            tag: Some(tag),
                        })
                        .collect(),
                    tag: Some(Tag {
                        property,
                        style: TagStyle::Consumed,
                    }),
                })
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
                    Some(reason) => tagged(resolved, view, rest, component_names, &reason, ctx, at),
                }
            }
        }
    };

    if nullable {
        return Shape::Optional(ShapeRef::Inline(Box::new(inner)));
    }
    inner
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
    /// Branches that only list strings, which read as one string enum ([`folded_strings`]).
    Strings(Vec<String>),
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
            _ => folded_strings(resolved, &rest).map_or(Self::Variants(rest), Self::Strings),
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
    branches: Vec<ShapeKey>,
    component_names: &ComponentNames,
    reason: &str,
    ctx: &mut Ctx,
    at: &JsonPointer,
) -> Shape {
    let inherited = view
        .discriminator
        .is_none()
        .then(|| shared_base_discriminator(resolved, &branches))
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

    let variants = branches
        .into_iter()
        .map(|key| {
            let tag = tag_value(resolved, declared, component_names, &key);
            Variant {
                shape: ShapeRef::Key(key),
                tag,
            }
        })
        .collect();
    Shape::Union(Union {
        variants,
        // Always proposed as consumed: [`super::discriminate`] may settle on carried instead,
        // once it can see how the variant types are used elsewhere.
        tag: Some(Tag {
            property: declared.property.clone(),
            style: TagStyle::Consumed,
        }),
    })
}

/// The property every branch declares as one string constant of its own, with each branch's
/// constant, when there is such a property.
///
/// `type: {enum: [creation]}` in one branch, `type: {enum: [update]}` in the next: a union of
/// objects that name themselves this way is discriminated by that property whether or not the
/// description spells `discriminator`, and is read the same way, by the tag's text. It cannot
/// be read by shape: the separation would rest on the constants, and a generated string enum is
/// open — it reads any string, so under it every branch fits every payload and the first one
/// wins. The first qualifying property in wire order is the tag, so the answer is the same
/// whichever branch is listed first.
fn constant_discriminator(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
) -> Option<(String, Vec<(ShapeKey, String)>)> {
    let views: Vec<(&ShapeKey, View)> = branches
        .iter()
        .map(|branch| (branch, merge::view(resolved, branch)))
        .collect();
    if views.len() < 2
        || views
            .iter()
            .any(|(_, view)| kind_of(resolved, view) != Some(Kind::Object))
    {
        return None;
    }
    let (_, first) = views.first()?;
    'candidates: for name in first.properties.keys() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut tags = Vec::new();
        for (key, view) in &views {
            let Some(property) = view.properties.get(name) else {
                continue 'candidates;
            };
            let Some(constant) = single_string_constant(resolved, property) else {
                continue 'candidates;
            };
            if !seen.insert(constant.clone()) {
                continue 'candidates;
            }
            tags.push(((*key).clone(), constant));
        }
        return Some((name.clone(), tags));
    }
    None
}

/// The one string a property is bound to, when `const` or a one-value `enum` binds it to one.
fn single_string_constant(resolved: &ResolvedDocument, key: &ShapeKey) -> Option<String> {
    let view = merge::view(resolved, key);
    let values = match (&view.enumeration, &view.constant) {
        (Some(listed), _) => listed.clone(),
        (None, Some(value)) => vec![value.clone()],
        (None, None) => return None,
    };
    let values: Vec<&Value> = values.iter().filter(|value| !value.is_null()).collect();
    match values.as_slice() {
        [one] => one.as_str().map(ToOwned::to_owned),
        _ => None,
    }
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
            .copied()
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
fn reads_strings_only(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    let Branch::Leaf(view) = Branch::of(resolved, key) else {
        return false;
    };
    kind_of(resolved, &view) == Some(Kind::String)
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
    let first = match Branch::of(resolved, earlier) {
        Branch::Through(inner) => return swallows_payloads_of(resolved, &inner, later, depth),
        Branch::Union(_) => {
            return Some(
                "one of its branches is itself a union and is not the last, and the type \
                 generated for it holds every payload its own branches do not fit, so it accepts \
                 the payloads meant for every branch after it"
                    .to_owned(),
            );
        }
        Branch::Leaf(view) => view,
    };
    let second = match Branch::of(resolved, later) {
        Branch::Through(inner) => return swallows_payloads_of(resolved, earlier, &inner, depth),
        Branch::Union(branches) => {
            return branches
                .iter()
                .find_map(|branch| swallows_payloads_of(resolved, earlier, branch, depth));
        }
        Branch::Leaf(view) => view,
    };

    // A branch that constrains nothing accepts every payload its siblings do — but an untagged
    // enum tries its variants in declaration order, so one sitting *last* is a catch-all that only
    // ever runs when everything else has failed. That is a shape real documents write on purpose
    // ("this list of things, or an untyped list"), and it loses nothing. Anywhere else the same
    // branch swallows the ones after it, which loses everything.
    let Some(left) = kind_of(resolved, &first) else {
        return Some(
            "one of its branches says nothing about the shape of its own payload and is not the \
             last, so it accepts the payloads meant for every branch after it"
                .to_owned(),
        );
    };
    let right = kind_of(resolved, &second)?;
    if left != right {
        return None;
    }
    match left {
        Kind::Object => (!rejects(resolved, &first, &second)).then(|| {
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
        // Two branches of one scalar kind read the same bytes into the same Rust type, so
        // whichever matches first is a faithful reading and nothing is lost.
        // That stops holding once the earlier branch lists its values.
        // No type generated for a list enforces it: a string enum is open, and a numeric or
        // boolean one is the plain scalar.
        // So the earlier branch takes every value of its kind, including the ones only a later
        // branch admits.
        Kind::String | Kind::Number | Kind::Boolean => {
            let listed = admitted_values(&first)?;
            (!admitted_values(&second).is_some_and(|later| later.is_subset(&listed))).then(|| {
                "one of its branches lists the values it admits, but the type generated for it \
                 reads any value of its kind, so it accepts the payloads of a later one, which is \
                 tried after it and so never reached"
                    .to_owned()
            })
        }
        Kind::Null => None,
    }
}

/// One branch of a union, as the type generated for it reads a payload.
enum Branch {
    /// A union of one branch and perhaps `null`, which reads exactly what that branch does.
    Through(ShapeKey),
    /// A union of two or more branches, which [`swallows_payloads_of`] compares by its branches.
    Union(Vec<ShapeKey>),
    /// Anything else, including a union of strings, which is one string enum.
    Leaf(Box<View>),
}

impl Branch {
    fn of(resolved: &ResolvedDocument, key: &ShapeKey) -> Self {
        let view = merge::view(resolved, key);
        // [`key`] gives up on these before it reads their branches, and [`kind_of`] reads them as
        // the arbitrary JSON they become.
        let degraded = view.impossible || view.conflict.is_some() || view.unions_collide;
        let Some(branches) = view.union.as_deref().filter(|_| !degraded) else {
            return Self::Leaf(Box::new(view));
        };
        let (reading, nullable) = Reading::of(resolved, branches);
        match reading {
            Reading::One(key) => Self::Through(key),
            Reading::Strings(values) => Self::Leaf(Box::new(View {
                types: Some(BTreeSet::from([TypeName::String])),
                enumeration: Some(values.into_iter().map(Value::String).collect()),
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
    match enumerated_kind(view) {
        // A bare enumeration names its kind as plainly as `type` would, and these three have a
        // Rust type of their own to be named into.
        Some(kind @ (Kind::String | Kind::Number | Kind::Boolean)) => return Some(kind),
        // An enumeration of objects or lists narrows nothing progeny can express, so the branch
        // becomes `serde_json::Value` and discriminates nothing.
        Some(Kind::Object | Kind::Array | Kind::Null) | None => {}
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
fn rejects(resolved: &ResolvedDocument, earlier: &View, later: &View) -> bool {
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
    if earlier
        .required
        .iter()
        .any(|name| insists(earlier, name) && !later.properties.contains_key(name))
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
        let (Some(here), Some(there)) = (
            kind_of(resolved, &merge::view(resolved, key)),
            kind_of(resolved, &merge::view(resolved, other)),
        ) else {
            return false;
        };
        here != there
    })
}

/// Whether the type generated for a schema is an `Option`, which reads an absent member as `None`.
///
/// Followed through a union of one branch, which is that branch's type, as far as
/// [`MAX_AMBIGUITY_DEPTH`]; an alias chain longer than that answers yes, which is the direction
/// that proves no separation.
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
    match Reading::of(resolved, branches) {
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

/// The strings a union's branches name, when every branch lists only strings.
///
/// A generated string enum is open, so one enum of every listed value reads each string the way
/// the document means it.
/// Kept as separate variants instead, each branch's enum would read every string, the first would
/// take them all, and the branches after it would never be reached.
///
/// A branch with something to report, or a union of its own, is not folded in: it is reported and
/// read where it is classified, and folded into the enum it would never be classified at all.
fn folded_strings(resolved: &ResolvedDocument, branches: &[ShapeKey]) -> Option<Vec<String>> {
    let mut values = Vec::new();
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
            (None, None) => return None,
        };
        for value in declared {
            values.push(value.as_str()?.to_owned());
        }
    }
    (!values.is_empty()).then_some(unique(values))
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

fn report_uninterpreted(view: &View, ctx: &mut Ctx, at: &JsonPointer) {
    if view.uninterpreted.is_empty() {
        return;
    }
    let keywords: Vec<String> = view
        .uninterpreted
        .iter()
        .map(|keyword| format!("`{keyword}`"))
        .collect();
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
