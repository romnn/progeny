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
//! really "is every variant type that declares the property used nowhere but here", and answering
//! it needs the whole shape graph.
//!
//! A union that may not consume its tag can still dispatch on it, provided every variant type
//! declares the property itself: the tag then stays *on the variants* ([`TagStyle::Carried`]),
//! nothing comes off any type, and the union reads the property without consuming it. Only when
//! that is impossible too — some variant does not declare the property, so a value of it would
//! lose its tag when serialized again — is the union demoted to [`Shape::Any`], not matched
//! structurally: `classify` only proposed a tag because matching by shape was already unsound, and
//! an unsound match does not become sound by having run out of alternatives.
//!
//! Unions that share a variant type constrain each other — a consumed union must not strip a
//! property that a carried union's payloads still put on the wire — so the settlement is a
//! fixpoint: every union starts consumed, each round re-evaluates all of them against the styles
//! of the rest, and a union only ever moves *away* from consumed, so the rounds run out of moves.

use std::collections::{BTreeMap, BTreeSet};

use super::{RootKind, Shape, ShapeKey, ShapeRef, Shapes, TagStyle, Union};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;

/// Settle every proposed tag: strip the variants of the consumed ones, mark the carried ones, and
/// demote the rest.
pub(super) fn run(resolved: &ResolvedDocument, shapes: &mut Shapes, ctx: &mut Ctx) {
    let proposed = proposals(shapes);
    if proposed.is_empty() {
        return;
    }
    let base_borrowed = borrowed_outside_tagged_unions(shapes);

    let mut modes: BTreeMap<ShapeKey, Mode> = proposed
        .iter()
        .map(|(key, _)| (key.clone(), Mode::Consumed))
        .collect();
    // Monotone, so it terminates: a round either flips at least one union away from `Consumed` —
    // and nothing ever flips back — or it changes nothing and is the fixpoint. The fixpoint
    // round's strips and warns are the ones reported and applied, because they are computed over
    // every union still consuming; a demotion's reason is kept from the round it flipped in,
    // which no later round revisits.
    let mut reasons: BTreeMap<ShapeKey, String> = BTreeMap::new();
    let outcome = loop {
        let round = evaluate(shapes, &proposed, &base_borrowed, &modes);
        for (key, reason) in &round.demoted {
            reasons.entry(key.clone()).or_insert_with(|| reason.clone());
        }
        if round.modes == modes {
            break round;
        }
        modes = round.modes.clone();
    };

    for (variant, property, first, union_key) in outcome.shared {
        // Both parents can have it: the tag is written by whichever enum is doing the
        // serializing, and the child needs it on neither. Worth saying out loud because a reader
        // of the generated types will find a property missing and want to know which unions took
        // it.
        //
        // Reported against the *variant*, which is the type that loses the property and so the
        // thing a reader has to go and look at. Against the union it would be several
        // byte-identical records whenever a union shares more than one variant with another —
        // which is what a per-occurrence class is supposed to never produce.
        ctx.report(
            Diagnostic::new(
                BreakageClass::MultiParentDiscriminator,
                Action::Warn,
                address(resolved, shapes, &variant),
                format!(
                    "`{property}` is the discriminator of more than one union over this variant; \
                     the variant carries the property in neither, because each union writes it \
                     itself"
                ),
            )
            .with_related(
                addresses(resolved, shapes, &first)
                    .into_iter()
                    .chain(addresses(resolved, shapes, &union_key)),
            ),
        );
    }
    // In proposal order rather than flip order, so which round a union fell in leaves no trace in
    // the report.
    for (union_key, _) in &proposed {
        if outcome.modes.get(union_key) != Some(&Mode::Demoted) {
            continue;
        }
        let Some(reason) = reasons.get(union_key) else {
            continue;
        };
        ctx.report(Diagnostic::new(
            BreakageClass::DiscriminatorEdgeCase,
            Action::Degrade,
            address(resolved, shapes, union_key),
            format!(
                "this union's variants cannot be told apart by their shape, and its declared \
                 discriminator cannot be consumed either because {reason}; the value is typed as \
                 arbitrary JSON rather than matched against whichever branch happens to parse first"
            ),
        ));
        shapes.replace(union_key, Shape::Any);
    }
    let carried: Vec<ShapeKey> = outcome
        .modes
        .iter()
        .filter(|(_, mode)| **mode == Mode::Carried)
        .map(|(key, _)| key.clone())
        .collect();
    for union_key in carried {
        let Some(Shape::Union(union)) = shapes.get(&union_key) else {
            continue;
        };
        let mut settled = union.clone();
        if let Some(tag) = settled.tag.as_mut() {
            tag.style = TagStyle::Carried;
        }
        shapes.replace(&union_key, Shape::Union(settled));
    }
    for (variant, property) in outcome.strip {
        shapes.strip_property(&variant, &property);
    }
}

/// What one union's proposal settled to, this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Consumed,
    Carried,
    Demoted,
}

/// One round's answers, deterministic in the modes it was asked against.
#[derive(Debug, Default)]
struct Round {
    modes: BTreeMap<ShapeKey, Mode>,
    /// Why each demoted union was, worded for the diagnostic.
    demoted: Vec<(ShapeKey, String)>,
    /// A variant two consumed unions dispatch over: `(variant, property, first union, this union)`.
    shared: Vec<(ShapeKey, String, ShapeKey, ShapeKey)>,
    strip: Vec<(ShapeKey, String)>,
}

/// Re-evaluate every proposal against the modes the previous round settled on.
///
/// `Carried` and `Demoted` are terminal: carried eligibility reads only what the variant types
/// declare, which no round changes, and a demotion is already the last resort. Only `Consumed`
/// entries are re-asked, because the borrowed set grows as unions turn carried.
fn evaluate(
    shapes: &Shapes,
    proposed: &[(ShapeKey, String)],
    base_borrowed: &BTreeSet<ShapeKey>,
    modes: &BTreeMap<ShapeKey, Mode>,
) -> Round {
    // A carried union's variant edges are wire positions where the tag property is genuinely in
    // the payload — that is what carrying means — so for this round they count as borrowed exactly
    // like a struct member does.
    let mut borrowed = base_borrowed.clone();
    for (key, mode) in modes {
        if *mode != Mode::Carried {
            continue;
        }
        if let Some(shape) = shapes.get(key) {
            for child in super::child_keys(shape) {
                borrowed.insert(child);
            }
        }
    }

    let mut round = Round {
        modes: modes.clone(),
        ..Round::default()
    };
    // Which property each variant type has already promised to give up, so a type shared by two
    // consumed unions is a finding rather than a race between them.
    let mut claimed: BTreeMap<ShapeKey, (String, ShapeKey)> = BTreeMap::new();

    for (union_key, property) in proposed {
        if modes.get(union_key) != Some(&Mode::Consumed) {
            continue;
        }
        let Some(Shape::Union(union)) = shapes.get(union_key) else {
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
            round.modes.insert(union_key.clone(), Mode::Demoted);
            round.demoted.push((
                union_key.clone(),
                "one of its variants is written inline rather than as a schema of its own"
                    .to_owned(),
            ));
            continue;
        }

        if let Some(reason) = refuse_outright(shapes, &named, property) {
            round.modes.insert(union_key.clone(), Mode::Demoted);
            round.demoted.push((union_key.clone(), reason));
            continue;
        }

        match refuse_consumption(shapes, &borrowed, &claimed, &named, property) {
            None => {
                for (variant, _) in &named {
                    if !declares(shapes, variant, property) {
                        // The union writes the tag and the variant never had the property, so
                        // there is nothing to take off it — and nothing another union could miss.
                        continue;
                    }
                    if let Some((_, first)) = claimed.get(variant) {
                        round.shared.push((
                            variant.clone(),
                            property.clone(),
                            first.clone(),
                            union_key.clone(),
                        ));
                    } else {
                        claimed.insert(variant.clone(), (property.clone(), union_key.clone()));
                        round.strip.push((variant.clone(), property.clone()));
                    }
                }
            }
            Some(reason) => match carried_shortfall(shapes, &named, property) {
                None => {
                    round.modes.insert(union_key.clone(), Mode::Carried);
                }
                Some(shortfall) => {
                    round.modes.insert(union_key.clone(), Mode::Demoted);
                    round.demoted.push((
                        union_key.clone(),
                        format!(
                            "{reason}, and the variants cannot carry the tag themselves because \
                             {shortfall}"
                        ),
                    ));
                }
            },
        }
    }
    round
}

/// Every union that asked for a tag, in a deterministic order.
fn proposals(shapes: &Shapes) -> Vec<(ShapeKey, String)> {
    shapes
        .entries()
        .filter_map(|(key, shape)| match shape {
            Shape::Union(Union { tag: Some(tag), .. }) => Some((key.clone(), tag.property.clone())),
            _ => None,
        })
        .collect()
}

/// Why no style can dispatch this union on its tag, if none can.
///
/// These are facts about the union and its variant types alone, the same whichever side would own
/// the property, so a union refused here is demoted without trying the other style.
fn refuse_outright(
    shapes: &Shapes,
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
        // A variant that declares the property under a type the tag cannot take is a defect worth
        // refusing over: a discriminator is always a string on the wire, whoever writes it.
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

/// Why this union may not *consume* its tag, if it may not.
///
/// Only variants that declare the property are asked: taking it off a type that never had it is
/// nothing at all, so such a variant constrains no one — the union writes the tag itself and the
/// type is the same everywhere it is used.
fn refuse_consumption(
    shapes: &Shapes,
    borrowed: &BTreeSet<ShapeKey>,
    claimed: &BTreeMap<ShapeKey, (String, ShapeKey)>,
    variants: &[(ShapeKey, Option<String>)],
    property: &str,
) -> Option<String> {
    for (variant, _) in variants {
        if !declares(shapes, variant, property) {
            continue;
        }
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
    }
    None
}

/// Why this union may not leave the tag on its variants, if it may not.
///
/// One requirement: every variant declares the property. Dispatch replays the whole payload into
/// the variant type, so a type that declares the property keeps it and writes it back; one that
/// does not would drop the tag on the way through, and a value serialized again would come out
/// undispatchable.
fn carried_shortfall(
    shapes: &Shapes,
    variants: &[(ShapeKey, Option<String>)],
    property: &str,
) -> Option<String> {
    for (variant, _) in variants {
        if !declares(shapes, variant, property) {
            return Some(format!(
                "one of them does not declare `{property}` and would lose the tag when serialized \
                 again"
            ));
        }
    }
    None
}

/// Whether a variant type declares the tag property at all.
fn declares(shapes: &Shapes, variant: &ShapeKey, property: &str) -> bool {
    match shapes.get(variant) {
        Some(Shape::Struct(structure)) => {
            structure.fields.iter().any(|field| field.wire == property)
        }
        _ => false,
    }
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
/// keep it. The exclusion is provisional: a proposal that settles on carrying its tag has its
/// variant edges added back, round by round, in [`evaluate`].
///
/// Edges between shapes are only half of it. A schema an *operation* names directly — a response
/// body, a request body, a parameter, a header — is a wire position with no edge pointing at it, so
/// a walk over shapes alone cannot see it, and taking the tag off a type that is *itself* a `200`
/// response makes the client drop the property coming in and omit it going out. So a
/// [`RootKind::Position`] root counts as a use, exactly like a struct member does.
///
/// Being *named* does not, because being named is not being on the wire: a `components.schemas`
/// entry, and equally a `components.responses` or `components.parameters` entry no operation
/// references, describes a payload nobody sends. Counting those would refuse every discriminated
/// union whose variants the document bothered to declare under `components`. Anything an operation
/// really does reference is reached through that operation and carries a `Position` root there, so
/// nothing on the wire is lost by ignoring the declaration.
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
    for root in shapes.roots() {
        if root.kind == RootKind::Position {
            borrowed.insert(root.key.clone());
        }
    }
    borrowed
}

/// Where a shape was written, for a diagnostic a reader navigates from.
///
/// Every part, not only the anchor. A merged key is the classification of several schemas at once
/// and takes its *name* from the first of them, which is deterministic and enough for a name — but
/// two different merges that share a first part then report at one pointer, and `influxdb` does
/// exactly that: four `multi-parent-discriminator` records whose related pointers read as the same
/// string twice, for what must be different unions. Harmless while a reader only counts them, and
/// actively misleading now that a pointer is meant to lead somewhere.
fn addresses(resolved: &ResolvedDocument, shapes: &Shapes, key: &ShapeKey) -> Vec<JsonPointer> {
    let mut out: Vec<JsonPointer> = key
        .parts()
        .iter()
        .map(|&id| resolved.schemas().address(id).clone())
        .collect();
    if out.is_empty() {
        out.push(address(resolved, shapes, key));
    }
    out
}

fn address(resolved: &ResolvedDocument, shapes: &Shapes, key: &ShapeKey) -> JsonPointer {
    shapes.address(key).cloned().unwrap_or_else(|| {
        key.anchor().map_or_else(JsonPointer::root, |id| {
            resolved.schemas().address(id).clone()
        })
    })
}
