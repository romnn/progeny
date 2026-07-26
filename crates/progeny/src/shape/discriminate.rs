//! Whether a proposed tag can actually be consumed, and what it costs the variants.
//!
//! [`super::classify`] proposes a tag for every union whose branches shape alone cannot tell
//! apart. It cannot *decide*, because the decision turns on facts no single schema knows.
//!
//! The cost is the reason. `#[serde(tag = "kind")]` takes the tag property out of the payload
//! before handing the rest to the variant, so a variant type that still declares `kind` would fail
//! to deserialize as a variant — the property has to come off the type. That is fine when the type
//! is only ever a variant of this union, and a silent loss when it is also used somewhere else,
//! because there the property really is on the wire. So the question "may this tag be consumed" is
//! really "is every one of these variant types used nowhere but here", and answering it needs the
//! whole shape graph.
//!
//! A union whose tag cannot be consumed is demoted to [`Shape::Any`], not matched structurally:
//! `classify` only proposed a tag because matching by shape was already unsound, and an unsound
//! match does not become sound by having run out of alternatives.

use std::collections::{BTreeMap, BTreeSet};

use super::{Shape, ShapeKey, ShapeRef, Shapes, Union};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;

/// Settle every proposed tag, strip the variants of the ones that survive, and demote the rest.
pub(super) fn run(resolved: &ResolvedDocument, shapes: &mut Shapes, ctx: &mut Ctx) {
    let proposed = proposals(shapes);
    if proposed.is_empty() {
        return;
    }
    let borrowed = borrowed_outside_tagged_unions(shapes);

    // Which property each variant type has already promised to give up, so a type shared by two
    // discriminated unions is a finding rather than a race between them.
    let mut claimed: BTreeMap<ShapeKey, (String, ShapeKey)> = BTreeMap::new();
    let mut strip: Vec<(ShapeKey, String)> = Vec::new();
    let mut demote: Vec<(ShapeKey, String)> = Vec::new();

    for (union_key, property) in proposed {
        let at = address(resolved, shapes, &union_key);
        let Some(Shape::Union(union)) = shapes.get(&union_key) else {
            continue;
        };
        let named: Vec<(ShapeKey, Option<String>)> = union
            .variants
            .iter()
            .filter_map(|variant| match &variant.shape {
                ShapeRef::Key(key) => Some((key.clone(), variant.tag.clone())),
                ShapeRef::Inline(_) => None,
            })
            .collect();
        if named.len() != union.variants.len() {
            demote.push((
                union_key,
                "one of its variants is written inline rather than as a schema of its own"
                    .to_owned(),
            ));
            continue;
        }
        let variants: Vec<ShapeKey> = named.iter().map(|(key, _)| key.clone()).collect();

        match refuse(shapes, &borrowed, &claimed, &named, &property) {
            Some(reason) => demote.push((union_key, reason)),
            None => {
                for variant in &variants {
                    if let Some((_, first)) = claimed.get(variant) {
                        // Both parents can have it: the tag is written by whichever enum is doing
                        // the serializing, and the child needs it on neither. Worth saying out
                        // loud because a reader of the generated types will find a property
                        // missing and want to know which unions took it.
                        ctx.report(
                            Diagnostic::new(
                                BreakageClass::MultiParentDiscriminator,
                                Action::Warn,
                                at.clone(),
                                format!(
                                    "`{property}` is the discriminator of more than one union over \
                                     this variant; the variant carries the property in neither, \
                                     because each union writes it itself"
                                ),
                            )
                            .with_related([address(resolved, shapes, first)]),
                        );
                    } else {
                        claimed.insert(variant.clone(), (property.clone(), union_key.clone()));
                        strip.push((variant.clone(), property.clone()));
                    }
                }
            }
        }
    }

    for (union_key, reason) in demote {
        ctx.report(Diagnostic::new(
            BreakageClass::DiscriminatorEdgeCase,
            Action::Degrade,
            address(resolved, shapes, &union_key),
            format!(
                "this union's variants cannot be told apart by their shape, and its declared \
                 discriminator cannot be consumed either because {reason}; the value is typed as \
                 arbitrary JSON rather than matched against whichever branch happens to parse first"
            ),
        ));
        shapes.replace(&union_key, Shape::Any);
    }
    for (variant, property) in strip {
        shapes.strip_property(&variant, &property);
    }
}

/// Every union that asked for a tag, in a deterministic order.
fn proposals(shapes: &Shapes) -> Vec<(ShapeKey, String)> {
    shapes
        .entries()
        .filter_map(|(key, shape)| match shape {
            Shape::Union(Union {
                tag: Some(property),
                ..
            }) => Some((key.clone(), property.clone())),
            _ => None,
        })
        .collect()
}

/// Why this union may not consume its tag, if it may not.
fn refuse(
    shapes: &Shapes,
    borrowed: &BTreeSet<ShapeKey>,
    claimed: &BTreeMap<ShapeKey, (String, ShapeKey)>,
    variants: &[(ShapeKey, Option<String>)],
    property: &str,
) -> Option<String> {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for (_, tag) in variants {
        // The resolution order — explicit mapping, then component name — has run out. Serde would
        // fall back to the Rust variant name, which is a string progeny made up rather than one
        // the document said, and writing it onto the wire would be exactly the invention this
        // whole layer exists to refuse.
        let Some(tag) = tag.as_deref() else {
            return Some(
                "one of its variants is named by neither the discriminator's mapping nor a \
                 component of its own, so nothing says what its tag would read"
                    .to_owned(),
            );
        };
        if !names.insert(tag) {
            return Some(format!(
                "two of its variants would both be tagged `{tag}`, so the tag would not say which \
                 of them a payload is"
            ));
        }
    }

    for (variant, _) in variants {
        let Some(Shape::Struct(structure)) = shapes.get(variant) else {
            return Some(
                "one of its variants is not an object, and a tag can only be carried inside one"
                    .to_owned(),
            );
        };
        if borrowed.contains(variant) {
            return Some(format!(
                "one of its variants is also used outside this union, where `{property}` really is \
                 on the wire, so taking the property off that type would lose it there"
            ));
        }
        if let Some((taken, _)) = claimed.get(variant)
            && taken != property
        {
            return Some(format!(
                "one of its variants already gives up `{taken}` to another union, and one type \
                 cannot carry two tags away"
            ));
        }
        // A variant that declares the property under a type the tag cannot take is a defect worth
        // refusing over: serde will write a string there whatever the schema said.
        if let Some(field) = structure.fields.iter().find(|field| field.wire == property)
            && !tag_shaped(shapes, &field.shape)
        {
            return Some(format!(
                "one of its variants declares `{property}` as something other than a string, and \
                 a discriminator is always a string on the wire"
            ));
        }
    }
    None
}

/// Whether a variant's own declaration of the tag property agrees that it is a string.
fn tag_shaped(shapes: &Shapes, reference: &ShapeRef) -> bool {
    let shape = match reference {
        ShapeRef::Key(key) => shapes.get(key),
        ShapeRef::Inline(shape) => Some(&**shape),
    };
    match shape {
        Some(Shape::Scalar(super::Scalar::String) | Shape::StringEnum(_) | Shape::Any) => true,
        // A nullable tag is still a string tag; the null is the document being loose, not a
        // different kind of value.
        Some(Shape::Optional(inner) | Shape::Alias(inner)) => tag_shaped(shapes, inner),
        _ => false,
    }
}

/// Every key reachable from a position that is not a variant of a union proposing a tag.
///
/// This is the whole safety condition. The variant edges of a *tagged* union are excluded because
/// those are exactly the positions where the tag is consumed rather than carried; every other
/// edge — a struct member, a list element, a map value, a variant of an untagged union — is a
/// position where the payload really does contain the property, so a type reached that way must
/// keep it.
fn borrowed_outside_tagged_unions(shapes: &Shapes) -> BTreeSet<ShapeKey> {
    let mut borrowed = BTreeSet::new();
    for (_, shape) in shapes.entries() {
        // A tagged union has no children but its variant edges, so exempting the shape exempts
        // exactly those.
        if matches!(shape, Shape::Union(Union { tag: Some(_), .. })) {
            continue;
        }
        for key in super::child_keys(shape) {
            borrowed.insert(key);
        }
    }
    borrowed
}

fn address(resolved: &ResolvedDocument, shapes: &Shapes, key: &ShapeKey) -> JsonPointer {
    shapes.address(key).cloned().unwrap_or_else(|| {
        key.anchor().map_or_else(JsonPointer::root, |id| {
            resolved.schemas().address(id).clone()
        })
    })
}
