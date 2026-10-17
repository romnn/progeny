//! Classification: schemas become one closed set of shapes.
//!
//! Every payload shape reachable from the API surface is classified exactly once, into an enum
//! small enough to reason about. This is where narrowness pays: the engine recognizes the shapes
//! real OpenAPI documents produce, and anything outside that set degrades to
//! [`Shape::Any`] — `serde_json::Value` — with a diagnostic, rather than growing machinery for a
//! construct nothing uses. There is no general fallback path, because generating `Value` loudly
//! *is* the designed behaviour.
//!
//! # Identity, and why a shape is keyed by a set of schemas
//!
//! A [`ShapeKey`] is the set of schemas whose constraints all have to hold. That one decision
//! carries three things that would otherwise each need their own mechanism:
//!
//! * **`$ref` is transparent.** `Foo: {$ref: Bar}` yields the same key as `Bar`, so a reference
//!   position uses the type the target names instead of a copy of it — without a `Reference` arm
//!   for every reader to remember to follow.
//! * **`allOf` is a merge, not an inheritance relation.** `{allOf: [{$ref: Base}, {…}]}` yields
//!   the key `{Base, …}`, and classifying that key merges both. 2020-12's rule that keywords
//!   beside a `$ref` also apply falls out of the same expansion.
//! * **Cycles cost nothing.** A key is a value, so a schema that refers to itself produces a key
//!   that has already been classified, and the walk stops without a visited set to maintain.
//!
//! Annotations deliberately do **not** make a schema part of a key: `{$ref: Pet, description: …}`
//! is by far the most common reference form in the corpus, and if a description forced a merge,
//! every one of those would become a distinct anonymous type instead of a reference to `Pet`.
//! Documentation is read at the position that carries it and travels with the field.
//!
//! One key is not a set of schemas alone: [`ShapeKey::branches`] is the union a set of schemas
//! declares, apart from the members declared beside it.
//! No schema states that union on its own, but a struct holding those members flattens it as a
//! type of its own, so it needs a key to be named and settled by.

mod classify;
mod discriminate;
mod fit;
mod merge;
mod roots;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::diag::{Ctx, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;

pub(crate) use fit::Fit;
pub(crate) use roots::{Root, RootKind};

/// The largest fixed-length array progeny emits.
///
/// `[T; N]` is a distinct type per length, and serde's own array impls stop being free above the
/// small sizes; past this a longer `minItems == maxItems` array is a `Vec`, which accepts a
/// superset — the sound direction.
const MAX_FIXED_ARRAY: u32 = 32;

/// The set of schemas a shape is the classification of.
///
/// Sorted and deduplicated, so that two documents writing the same `allOf` in different orders
/// produce one key and therefore one type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ShapeKey {
    parts: Vec<SchemaId>,
    /// Whether the key stands for only the `oneOf`/`anyOf` its schemas declare, without the
    /// members they declare beside it.
    ///
    /// No schema in the document states that union on its own, but a struct holding those members
    /// flattens it as a type of its own, which has to be named and settled like any other.
    branches: bool,
}

impl ShapeKey {
    fn new(mut parts: Vec<SchemaId>) -> Self {
        parts.sort_unstable();
        parts.dedup();
        Self {
            parts,
            branches: false,
        }
    }

    pub(crate) fn parts(&self) -> &[SchemaId] {
        &self.parts
    }

    /// The schema that gives this shape its address: the first part, in id order.
    ///
    /// Ids are assigned in document order, so this is the earliest-written schema of the set — a
    /// deterministic choice that does not depend on which position reached the key first.
    pub(crate) fn anchor(&self) -> Option<SchemaId> {
        self.parts.first().copied()
    }

    /// The key of the `oneOf`/`anyOf` these schemas declare, apart from their own members.
    pub(crate) fn branches(&self) -> Self {
        Self {
            parts: self.parts.clone(),
            branches: true,
        }
    }

    /// Whether this key stands for a union apart from the members declared beside it.
    pub(crate) fn is_branches(&self) -> bool {
        self.branches
    }
}

/// A child position: another shape by key, or one derived from this schema alone.
///
/// Most children are keys — a property's type is another schema's shape. Some are not: the inner
/// shape of `type: ["string", "null"]` is "this schema without the null", which no schema in the
/// document states on its own and which therefore has no key to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShapeRef {
    Key(ShapeKey),
    Inline(Box<Shape>),
}

/// What one schema is, as far as the type layer is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    /// `type: "null"`, which is a type with one value.
    Null,
    Scalar(Scalar),
    /// A string whose `format` progeny gives a type of its own.
    Format(Format),
    /// An `enum` or `const` of strings: a fieldless enum, and the serde path with no buffering.
    StringEnum(Vec<String>),
    Struct(Struct),
    /// An object with no declared property names.
    Map {
        value: Option<ShapeRef>,
    },
    Array {
        item: Option<ShapeRef>,
    },
    /// `prefixItems`: fixed positions, each with its own type.
    ///
    /// Always fixed-arity: a prefix followed by an `items` that admits values is degraded by the
    /// classifier, because instances could then be longer than the prefix and no fixed-arity Rust
    /// tuple accepts that. A `rest` field once carried that tail here, and nothing ever lowered
    /// it — the arity claim lives in the classifier now, not in a field renderers must remember
    /// to read.
    Tuple {
        items: Vec<ShapeRef>,
    },
    /// `minItems == maxItems` over a uniform element type.
    FixedArray {
        item: ShapeRef,
        len: u32,
    },
    Union(Union),
    /// serde's externally tagged enum: a listed string, or an object whose one member's key
    /// names the variant and whose value is its payload.
    Keyed(Keyed),
    /// A shape that also admits `null`.
    Optional(ShapeRef),
    /// A shape that is exactly another one.
    ///
    /// Produced by a union with a single branch, and by the nullable-emulation pattern once the
    /// `null` branch has been peeled off. A named position holding one of these renders as a type
    /// alias, which is why it is a shape rather than something the lowering has to invent.
    Alias(ShapeRef),
    /// The degradation target: `serde_json::Value`, which holds anything a document can say.
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scalar {
    Bool,
    /// Signedness comes from the bounds; width never does.
    ///
    /// 21% of the corpus's integers carry a bound, which looks like an argument for narrowing —
    /// but a width chosen from `maximum` is a forward-compatibility hazard, not an optimization:
    /// it costs nothing to store an `i64`, and a vendor raising a documented ceiling would turn
    /// previously-valid responses into deserialization failures. `minimum >= 0` is different in
    /// kind: it says the value has no sign, which a vendor cannot widen without a breaking change.
    Integer {
        signed: bool,
    },
    Number,
    String,
}

/// The string formats progeny gives a type other than `String`.
///
/// Closed and short on purpose. Every other `format` — `uri`, `email`, `hostname` — maps to
/// `String` in v1, so recognizing it would add a variant that changes nothing. `format` is an
/// annotation rather than an assertion, and the model keeps the spelling either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Format {
    DateTime,
    Date,
    Time,
    Uuid,
    Ip,
    Ipv4,
    Ipv6,
    /// `contentEncoding: base64`, or 3.0's `format: byte`.
    Base64,
    /// `contentMediaType: application/octet-stream`, or 3.0's `format: binary`.
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Struct {
    pub(crate) fields: Vec<Field>,
    /// What a property the document did not name may be.
    pub(crate) extra: Extra,
    /// The union declared beside the fields, which holds every member they do not name.
    ///
    /// `{properties: P, anyOf: [A, B]}` is `P` and one of `A` or `B`, so the fields read `P`'s
    /// members and the union reads the rest.
    /// Set only where no branch names a member of `P`, so each member has exactly one reader, and
    /// [`Struct::extra`] is then [`Extra::Open`].
    pub(crate) flattened: Option<ShapeRef>,
}

/// The policy for properties the schema does not name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Extra {
    /// `additionalProperties` absent: anything may appear, and the document does not say what.
    Open,
    /// `additionalProperties: false`.
    Denied,
    /// `additionalProperties: <schema>`, or a uniform `patternProperties`.
    Typed(ShapeRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    /// The property name, exactly as it appears on the wire.
    pub(crate) wire: String,
    pub(crate) shape: ShapeRef,
    pub(crate) required: bool,
    pub(crate) docs: Docs,
    /// The declared default, still unvalidated: the contract layer checks it against the type and
    /// drops it with a diagnostic if it does not fit.
    pub(crate) default: Option<Value>,
    pub(crate) read_only: bool,
    pub(crate) write_only: bool,
}

/// One branch of a union, with the name the document suggests for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Variant {
    pub(crate) shape: ShapeRef,
    /// The value this variant's tag property takes, for a union that carries its tag internally.
    ///
    /// Set exactly when the enclosing [`Union::tag`] is set. It comes from the discriminator's
    /// explicit mapping where there is one and from the variant's component name otherwise, which
    /// is the resolution order OpenAPI defines.
    pub(crate) tag: Option<String>,
    /// Other values of the tag property that name this variant too, which the union reads and
    /// never writes in place of [`Variant::tag`].
    ///
    /// Set only for a union whose branches name themselves, where one branch lists several
    /// spellings of its own name, as `tool_search_tool_bm25` beside
    /// `tool_search_tool_bm25_20251119`.
    pub(crate) aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Union {
    pub(crate) variants: Vec<Variant>,
    /// The property that says which variant a payload is, for a union progeny dispatches by tag.
    ///
    /// `None` is the common case and means the variants are told apart by their shape. A union is
    /// only tagged when structural matching would be *unsound* — see [`super::discriminate`] —
    /// because a tag has a cost either way: consuming it costs the variant types the property
    /// itself, and carrying it costs every variant a member the builder of a value must fill in.
    /// Paying either when the shapes already distinguish the variants would be a loss for nothing.
    pub(crate) tag: Option<Tag>,
}

impl Union {
    /// The variant a tagged payload is, from its tag member: `None` for the member being absent,
    /// and otherwise its value.
    ///
    /// A missing or `null` member is the variant [`Tag::absent`] or [`Tag::null`] names, a string
    /// is the variant whose tag or alias it is, and any other value names no variant.
    pub(crate) fn tagged_variant(&self, member: Option<&Value>) -> Option<&Variant> {
        self.variants.get(self.tagged_position(member)?)
    }

    /// The position of the variant [`Union::tagged_variant`] finds.
    pub(crate) fn tagged_position(&self, member: Option<&Value>) -> Option<usize> {
        let tag = self.tag.as_ref()?;
        match member {
            None => tag.absent,
            Some(Value::Null) => tag.null,
            Some(Value::String(named)) => self.variants.iter().position(|variant| {
                variant.tag.as_deref() == Some(named.as_str())
                    || variant.aliases.iter().any(|alias| alias == named)
            }),
            Some(_) => None,
        }
    }
}

/// A union whose branches are listed strings and single-member objects, each named on the wire.
///
/// serde writes a Rust enum this way by default, and `schemars` exports it as such a union.
/// The string or the one key is the variant's name, so the variants need no shape to tell them
/// apart and no wrapper type to hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keyed {
    pub(crate) variants: Vec<KeyedVariant>,
    /// Whether a last branch accepts any other string and any other object, which the open arm
    /// holds.
    pub(crate) open: bool,
}

/// One variant of a [`Keyed`] union, named by the string or key the wire carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyedVariant {
    /// The string, or the object's one key.
    pub(crate) name: String,
    /// The value under the key, or `None` for a variant written as its bare string.
    ///
    /// `{"custom": null}` has a payload of `null` and is not the string `"custom"`.
    pub(crate) payload: Option<ShapeRef>,
}

/// The tag of a union whose variants shape alone cannot tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag {
    /// The property the payload names its own variant in.
    pub(crate) property: String,
    pub(crate) style: TagStyle,
    /// The position of the one variant a payload without the tag property is.
    ///
    /// Set where exactly one branch leaves the property optional and every other branch requires
    /// it, so a payload that omits it can only be that branch.
    pub(crate) absent: Option<usize>,
    /// The position of the one variant a payload whose tag property is `null` is.
    ///
    /// Set where exactly one branch admits `null` there, for the same reason.
    pub(crate) null: Option<usize>,
}

impl Tag {
    /// A tag every payload carries as one of the variants' listed values.
    pub(crate) fn consumed(property: impl Into<String>) -> Self {
        Self {
            property: property.into(),
            style: TagStyle::Consumed,
            absent: None,
            null: None,
        }
    }

    /// Whether some payload names its variant other than by the one value the union writes.
    ///
    /// Such a union has to leave the tag on its variants, because a union that consumes the tag
    /// writes it back as that one value, which would change an alias and fill in an absent tag.
    pub(crate) fn reads_more_than_it_writes(&self, union: &Union) -> bool {
        self.absent.is_some()
            || self.null.is_some()
            || union
                .variants
                .iter()
                .any(|variant| !variant.aliases.is_empty())
    }
}

/// Which side of a tagged union owns the tag property.
///
/// [`super::classify`] always proposes `Consumed`; [`super::discriminate`] settles the style,
/// because the choice turns on how the variant types are used elsewhere — a fact no single schema
/// knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagStyle {
    /// The union owns the property: taken out of the payload before the variant type reads it,
    /// written by the union when serializing. The variant types do not declare it.
    Consumed,
    /// Every variant type declares the property and keeps it: the union reads it to pick a
    /// variant, then replays the whole payload — tag included — into that variant's type, and
    /// serializing writes the variant with the tag that names it.
    /// Settled when consuming would take the property off a type that carries it somewhere else
    /// on the wire, and when the union reads a tag it would not write back — an alias, or a
    /// missing or `null` tag — because only the variant can remember which of those it had.
    Carried,
}

/// What a schema says about itself, for the doc comments a renderer writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Docs {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) deprecated: bool,
}

impl Docs {
    pub(crate) fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && !self.deprecated
    }
}

/// Every shape a document produces, and the positions that need names.
#[derive(Debug, Default)]
pub(crate) struct Shapes {
    by_key: BTreeMap<ShapeKey, Shape>,
    roots: Vec<Root>,
    /// Where each key's shape was written, for naming and diagnostics.
    addresses: BTreeMap<ShapeKey, JsonPointer>,
}

impl Shapes {
    pub(crate) fn get(&self, key: &ShapeKey) -> Option<&Shape> {
        self.by_key.get(key)
    }

    pub(crate) fn roots(&self) -> &[Root] {
        &self.roots
    }

    pub(crate) fn address(&self, key: &ShapeKey) -> Option<&JsonPointer> {
        self.addresses.get(key)
    }

    /// Every key, in a deterministic order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &ShapeKey> {
        self.by_key.keys()
    }

    /// Every key and its shape, in a deterministic order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&ShapeKey, &Shape)> {
        self.by_key.iter()
    }

    /// Replace what a key classified as.
    ///
    /// The one mutation classification allows after the fact, and it exists for one caller:
    /// [`discriminate`] can only tell whether a proposed tag is affordable once every shape is
    /// known, so the union it demotes was necessarily classified before the answer existed.
    pub(crate) fn replace(&mut self, key: &ShapeKey, shape: Shape) {
        let slot = self.by_key.get_mut(key);
        // Not a tolerable miss: `replace` is how an unsound union is demoted, and a demotion
        // that silently missed leaves an untagged enum matching by declaration order — the
        // WildUnion failure. Every caller today iterates `entries()`, so the key exists; this
        // holds that reasoning against a future caller that derives the key instead.
        debug_assert!(
            slot.is_some(),
            "replacing a shape that was never classified"
        );
        if let Some(slot) = slot {
            *slot = shape;
        }
    }

    /// Drop a property from a struct, because the union above it now carries that property itself.
    ///
    /// A no-op on anything that is not a struct with that member, so a caller that has already
    /// stripped the property — a variant shared by two unions with the same tag — stays correct.
    pub(crate) fn strip_property(&mut self, key: &ShapeKey, wire: &str) {
        if let Some(Shape::Struct(structure)) = self.by_key.get_mut(key) {
            structure.fields.retain(|field| field.wire != wire);
        }
    }
}

/// Classify every shape the document's API surface reaches.
///
/// Iterative rather than recursive: a document can nest schemas 128 deep and reference in cycles,
/// and the work list makes both ordinary rather than a stack-depth question.
pub(crate) fn classify(resolved: &ResolvedDocument, ctx: &mut Ctx) -> Shapes {
    let roots = roots::discover(resolved);
    let mut shapes = Shapes {
        by_key: BTreeMap::new(),
        roots: Vec::new(),
        addresses: BTreeMap::new(),
    };

    let mut queue: Vec<ShapeKey> = Vec::new();
    let mut named = Vec::new();
    // The name `components.schemas` gave each shape, which is the discriminator's implicit
    // mapping. Collected from the roots because their keys are computed here anyway, and because
    // a variant's own name is not recoverable from its key: `allOf: [$ref Base, {…}]` contributes
    // nothing of its own, so the key holds the base's address and the extra part's, and neither
    // is the name the document knows the variant by.
    let mut component_names: classify::ComponentNames = BTreeMap::new();
    for site in roots {
        let key = merge::key_of(resolved, site.id);
        if site.kind == RootKind::Component
            && let Some(name) = site.hint.first()
        {
            component_names.entry(key.clone()).or_insert(name.clone());
        }
        queue.push(key.clone());
        named.push(Root {
            key,
            hint: site.hint,
            kind: site.kind,
        });
    }
    shapes.roots = named;

    while let Some(key) = queue.pop() {
        if shapes.by_key.contains_key(&key) {
            continue;
        }
        let shape = classify::key(resolved, &key, &component_names, ctx);
        children(&shape, &mut queue);
        if let Some(anchor) = key.anchor() {
            shapes
                .addresses
                .insert(key.clone(), resolved.schemas().address(anchor).clone());
        }
        shapes.by_key.insert(key, shape);
    }
    // Every shape exists now, which is the earliest moment a proposed discriminator tag can be
    // priced: what it costs is a property off each variant type, and whether that is affordable
    // depends on where else those types are used.
    discriminate::run(resolved, &mut shapes, ctx);
    shapes
}

/// What a shape's own schemas say about themselves, for the doc comments a renderer writes.
pub(crate) fn docs_of(resolved: &ResolvedDocument, key: &ShapeKey) -> Docs {
    classify::docs(resolved, key)
}

/// The key a schema classifies under.
///
/// The API model's way in: an operation holds a `SchemaId` and needs the shape — and therefore the
/// type — that id became. Going through the same function classification used is what keeps the two
/// from disagreeing about which schemas are one shape.
pub(crate) fn key_of(resolved: &ResolvedDocument, id: SchemaId) -> ShapeKey {
    merge::key_of(resolved, id)
}

/// Every key a shape refers to.
pub(crate) fn child_keys(shape: &Shape) -> Vec<ShapeKey> {
    let mut out = Vec::new();
    children(shape, &mut out);
    out
}

/// Every key a shape refers to, so the work list can reach it.
fn children(shape: &Shape, out: &mut Vec<ShapeKey>) {
    let mut push = |reference: &ShapeRef| match reference {
        ShapeRef::Key(key) => out.push(key.clone()),
        // An inline shape is classified already; its own children still have to be reached.
        ShapeRef::Inline(inner) => children(inner, out),
    };
    match shape {
        Shape::Null | Shape::Scalar(_) | Shape::Format(_) | Shape::StringEnum(_) | Shape::Any => {}
        Shape::Struct(structure) => {
            for field in &structure.fields {
                push(&field.shape);
            }
            if let Extra::Typed(extra) = &structure.extra {
                push(extra);
            }
            if let Some(flattened) = &structure.flattened {
                push(flattened);
            }
        }
        Shape::Map { value } | Shape::Array { item: value } => {
            if let Some(value) = value {
                push(value);
            }
        }
        Shape::Tuple { items } => {
            for item in items {
                push(item);
            }
        }
        Shape::FixedArray { item: inner, .. } | Shape::Alias(inner) | Shape::Optional(inner) => {
            push(inner);
        }
        Shape::Union(union) => {
            for variant in &union.variants {
                push(&variant.shape);
            }
        }
        Shape::Keyed(keyed) => {
            for payload in keyed
                .variants
                .iter()
                .filter_map(|variant| variant.payload.as_ref())
            {
                push(payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{Extra, Scalar, Shape, ShapeRef, Shapes, Tag, TagStyle, Union, classify};
    use crate::diag::{BreakageClass, Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::{normalize, resolve};

    /// Classify a document, keeping the diagnostics.
    pub(super) fn shapes_of(document: Value) -> eyre::Result<(Shapes, Vec<Diagnostic>)> {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = classify(&resolved, &mut ctx);
        Ok((shapes, ctx.into_diagnostics()))
    }

    /// A document whose `components.schemas` are exactly these.
    pub(super) fn with_schemas(schemas: Value) -> Value {
        // Built member by member rather than with `json!`, which would only borrow `schemas`.
        let mut components = serde_json::Map::new();
        components.insert("schemas".to_owned(), schemas);
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), Value::Object(serde_json::Map::new()));
        root.insert("components".to_owned(), Value::Object(components));
        Value::Object(root)
    }

    /// The shape of the component named `name`.
    pub(super) fn shape_of<'a>(shapes: &'a Shapes, name: &str) -> eyre::Result<&'a Shape> {
        let root = shapes
            .roots()
            .iter()
            .find(|root| root.hint.iter().any(|segment| segment == name))
            .ok_or_else(|| eyre::eyre!("no root named {name}"))?;
        shapes
            .get(&root.key)
            .ok_or_else(|| eyre::eyre!("no shape for root named {name}"))
    }

    /// The polymorphism spelling from OpenAPI's own guide, and the two things it needs: the base
    /// becomes the union its `mapping` names, and a `oneOf` over the same family inherits the
    /// discriminator the family's base declares rather than degrading for want of one.
    #[test_util::test]
    fn the_inheritance_spelling_of_a_union_is_read_as_one() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Pet": {
                "type": "object",
                "properties": {"petType": {"type": "string"}},
                "required": ["petType"],
                "discriminator": {
                    "propertyName": "petType",
                    "mapping": {
                        "cat": "#/components/schemas/Cat",
                        "dog": "#/components/schemas/Dog",
                    },
                },
            },
            "Cat": {"allOf": [
                {"$ref": "#/components/schemas/Pet"},
                {"type": "object", "properties": {"lives": {"type": "integer"}}},
            ]},
            "Dog": {"allOf": [
                {"$ref": "#/components/schemas/Pet"},
                {"type": "object", "properties": {"pack": {"type": "boolean"}}},
            ]},
            "Sighting": {
                "type": "object",
                "properties": {
                    "seen": {"oneOf": [
                        {"$ref": "#/components/schemas/Cat"},
                        {"$ref": "#/components/schemas/Dog"},
                    ]},
                },
            },
        })))?;
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.class() == BreakageClass::InheritanceDiscriminator),
            "{diagnostics:?}"
        );
        // Nothing degrades: the `oneOf` under `Sighting` would be a wild union without the tag it
        // inherits, and the base would be a struct holding `petType` and nothing else.
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.class() == BreakageClass::WildUnion),
            "{diagnostics:?}"
        );

        let tags = |shape: &Shape| match shape {
            Shape::Union(union) => (
                union.tag.clone(),
                union
                    .variants
                    .iter()
                    .map(|variant| variant.tag.clone())
                    .collect::<Vec<_>>(),
            ),
            other => panic!("expected a union, found {other:?}"),
        };
        let want = (
            Some(Tag {
                property: "petType".to_owned(),
                style: TagStyle::Consumed,
                absent: None,
                null: None,
            }),
            vec![Some("cat".to_owned()), Some("dog".to_owned())],
        );
        assert_eq!(tags(shape_of(&shapes, "Pet")?), want);

        let seen = match shape_of(&shapes, "Sighting")? {
            Shape::Struct(structure) => structure
                .fields
                .iter()
                .find(|field| field.wire == "seen")
                .map(|field| field.shape.clone())
                .ok_or_eyre("the fixture declares this member")?,
            other => panic!("expected a struct, found {other:?}"),
        };
        let ShapeRef::Key(seen) = seen else {
            panic!("the member is a reference to a named shape");
        };
        let seen = shapes.get(&seen).ok_or_eyre("the shape was classified")?;
        assert_eq!(tags(seen), want);

        // The tag lives on the union, so it comes off the variants: a variant that still declared
        // `petType` could not deserialize as one.
        match shape_of(&shapes, "Cat")? {
            Shape::Struct(structure) => assert_eq!(
                structure
                    .fields
                    .iter()
                    .map(|field| field.wire.as_str())
                    .collect::<Vec<_>>(),
                ["lives"]
            ),
            other => panic!("expected a struct, found {other:?}"),
        }
    }

    #[test_util::test]
    fn a_reference_is_transparent_so_both_positions_share_one_type() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Alias": {"$ref": "#/components/schemas/Pet"},
        })))?;
        let pet = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Pet"])
            .ok_or_eyre("test fixture should contain this value")?;
        let alias = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Alias"])
            .ok_or_eyre("test fixture should contain this value")?;
        // Not two structurally equal types that dedup has to notice afterwards: one key.
        assert_eq!(pet.key, alias.key);
    }

    #[test_util::test]
    fn a_reference_with_a_description_beside_it_is_still_a_reference() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Holder": {
                "type": "object",
                "properties": {
                    "pet": {"$ref": "#/components/schemas/Pet", "description": "the pet"},
                },
            },
        })))?;
        let pet = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Pet"])
            .ok_or_eyre("test fixture should contain this value")?;
        let Shape::Struct(holder) = shape_of(&shapes, "Holder")? else {
            panic!("expected a struct");
        };
        assert_eq!(holder.fields[0].shape, ShapeRef::Key(pet.key.clone()));
        // The description travels with the field rather than making a new type.
        assert_eq!(
            holder.fields[0].docs.description.as_deref(),
            Some("the pet")
        );
    }

    #[test_util::test]
    fn a_recursive_schema_is_classified_once_and_terminates() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Node": {
                "type": "object",
                "properties": {"children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}}},
            },
        })))?;
        let Shape::Struct(node) = shape_of(&shapes, "Node")? else {
            panic!("expected a struct");
        };
        let ShapeRef::Key(children) = &node.fields[0].shape else {
            panic!("expected a key");
        };
        let Some(Shape::Array { item: Some(item) }) = shapes.get(children) else {
            panic!("expected an array");
        };
        // The element's key is the struct's own: the cycle is two entries in a map, not a loop.
        assert_eq!(item, &ShapeRef::Key(shape_key_of(&shapes, "Node")?));
    }

    fn shape_key_of(shapes: &Shapes, name: &str) -> eyre::Result<super::ShapeKey> {
        let root = shapes
            .roots()
            .iter()
            .find(|root| root.hint.iter().any(|segment| segment == name))
            .ok_or_else(|| eyre::eyre!("no root named {name}"))?
            .key
            .clone();
        Ok(root)
    }

    #[test_util::test]
    fn an_open_object_with_no_properties_is_a_map() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Bag": {"type": "object"},
            "Typed": {"type": "object", "additionalProperties": {"type": "integer"}},
        })))?;
        assert!(matches!(
            shape_of(&shapes, "Bag")?,
            Shape::Map { value: None }
        ));
        assert!(matches!(
            shape_of(&shapes, "Typed")?,
            Shape::Map { value: Some(_) }
        ));
    }

    #[test_util::test]
    fn additional_properties_false_is_recorded_rather_than_ignored() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Closed": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": false,
            },
        })))?;
        let Shape::Struct(closed) = shape_of(&shapes, "Closed")? else {
            panic!("expected a struct");
        };
        assert_eq!(closed.extra, Extra::Denied);
    }

    #[test_util::test]
    fn all_of_unions_the_properties_and_the_required_names() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
            },
            "Child": {
                "allOf": [
                    {"$ref": "#/components/schemas/Base"},
                    {"type": "object", "required": ["extra"], "properties": {"extra": {"type": "integer"}}},
                ],
            },
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Struct(child) = shape_of(&shapes, "Child")? else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = child
            .fields
            .iter()
            .map(|field| field.wire.as_str())
            .collect();
        assert_eq!(names, ["extra", "id"]);
        assert!(child.fields.iter().all(|field| field.required));
    }

    #[test_util::test]
    fn a_property_two_branches_disagree_about_is_merged_one_level_down() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Merged": {
                "allOf": [
                    {"properties": {"a": {"type": "object", "properties": {"x": {"type": "string"}}}}},
                    {"properties": {"a": {"type": "object", "properties": {"y": {"type": "string"}}}}},
                ],
            },
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Struct(merged) = shape_of(&shapes, "Merged")? else {
            panic!("expected a struct");
        };
        let ShapeRef::Key(key) = &merged.fields[0].shape else {
            panic!("expected a key");
        };
        // Both statements about `a` hold, so its type has both properties.
        let Some(Shape::Struct(inner)) = shapes.get(key) else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = inner
            .fields
            .iter()
            .map(|field| field.wire.as_str())
            .collect();
        assert_eq!(names, ["x", "y"]);
    }

    #[test_util::test]
    fn an_integer_that_must_also_be_a_number_is_an_integer() {
        // `integer` is a subset of `number`, which the type names hide. `github` writes exactly this
        // and it is not a contradiction.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Id": {"allOf": [{"type": "number"}, {"type": "integer"}]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            shape_of(&shapes, "Id")?,
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
    }

    #[test_util::test]
    fn an_all_of_that_cannot_hold_degrades_and_names_the_conflict() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Impossible": {"allOf": [{"type": "string"}, {"type": "integer"}]},
        })))?;
        assert_eq!(shape_of(&shapes, "Impossible")?, &Shape::Any);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].class(),
            crate::BreakageClass::IrreconcilableAllOf
        );
        assert!(diagnostics[0].detail().contains("no value satisfies both"));
    }

    #[test_util::test]
    fn the_nullable_emulation_pattern_is_an_option_and_not_a_union() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "MaybePet": {"anyOf": [{"$ref": "#/components/schemas/Pet"}, {"type": "null"}]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Optional(ShapeRef::Inline(inner)) = shape_of(&shapes, "MaybePet")? else {
            panic!("expected an optional");
        };
        // 83% of every `anyOf` in the corpus is this, and it has an exact translation.
        assert!(matches!(**inner, Shape::Alias(ShapeRef::Key(_))));
    }

    #[test_util::test]
    fn a_union_of_constants_is_a_fieldless_enum() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Colour": {"anyOf": [{"const": "red"}, {"const": "green"}]},
            "Direction": {"enum": ["up", "down"]},
        })))?;
        assert_eq!(
            shape_of(&shapes, "Colour")?,
            &Shape::StringEnum(vec!["red".to_owned(), "green".to_owned()])
        );
        assert_eq!(
            shape_of(&shapes, "Direction")?,
            &Shape::StringEnum(vec!["up".to_owned(), "down".to_owned()])
        );
    }

    #[test_util::test]
    fn a_nullable_enum_is_an_optional_fieldless_enum() {
        // How 3.0's `nullable: true` beside an `enum` arrives after normalization.
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Colour": {"type": ["string", "null"], "enum": ["red", null]},
        })))?;
        let Shape::Optional(ShapeRef::Inline(inner)) = shape_of(&shapes, "Colour")? else {
            panic!("expected an optional");
        };
        assert_eq!(**inner, Shape::StringEnum(vec!["red".to_owned()]));
    }

    #[test_util::test]
    fn an_undiscriminated_one_of_is_matched_structurally() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            // A second member keeps each branch from being read by its one key.
            "Cat": {"type": "object", "required": ["meow"], "properties": {
                "meow": {"type": "string"}, "name": {"type": "string"},
            }},
            "Dog": {"type": "object", "required": ["woof"], "properties": {
                "woof": {"type": "string"}, "name": {"type": "string"},
            }},
            "Animal": {"oneOf": [
                {"$ref": "#/components/schemas/Cat"},
                {"$ref": "#/components/schemas/Dog"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(animal) = shape_of(&shapes, "Animal")? else {
            panic!("expected a union");
        };
        assert_eq!(animal.variants.len(), 2);
    }

    #[test_util::test]
    fn a_discriminated_union_whose_variants_look_alike_consumes_its_tag() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(either) = shape_of(&shapes, "Either")? else {
            panic!("expected a union");
        };
        // Matching these structurally would pick whichever deserialized first, so the tag is what
        // decides, and the mapping is implicit: a variant is known by its component name.
        assert_eq!(
            either.tag,
            Some(Tag {
                property: "kind".to_owned(),
                style: TagStyle::Consumed,
                absent: None,
                null: None,
            })
        );
        let tags: Vec<Option<&str>> = either
            .variants
            .iter()
            .map(|variant| variant.tag.as_deref())
            .collect();
        assert_eq!(tags, [Some("A"), Some("B")]);
        // And the property the tag rides in comes off the variants, because serde consumes it
        // before the variant ever sees the payload.
        let Shape::Struct(a) = shape_of(&shapes, "A")? else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = a.fields.iter().map(|field| field.wire.as_str()).collect();
        assert_eq!(names, ["shared"]);
    }

    /// Branches that each bind one property to a string constant of their own name themselves,
    /// and the union reads that property as its tag — with or without a `discriminator`. It
    /// cannot read them by shape: a generated string enum is open, so under it every branch
    /// would fit every payload and the first would win.
    #[test_util::test]
    fn a_constant_shared_by_every_branch_is_the_tag_whether_or_not_it_is_declared() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Cat": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"const": "cat"}, "shared": {"type": "string"}},
            },
            "Dog": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"enum": ["dog"]}, "shared": {"type": "string"}},
            },
            "Animal": {
                "oneOf": [
                    {"$ref": "#/components/schemas/Cat"},
                    {"$ref": "#/components/schemas/Dog"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            "Fish": {
                "type": "object",
                "properties": {"kind": {"const": "fish"}, "fins": {"type": "integer"}},
            },
            "Bird": {
                "type": "object",
                "properties": {"kind": {"const": "bird"}, "fins": {"type": "integer"}},
            },
            "Undeclared": {
                "oneOf": [
                    {"$ref": "#/components/schemas/Fish"},
                    {"$ref": "#/components/schemas/Bird"},
                ],
            },
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (name, expected) in [
            ("Animal", [Some("cat"), Some("dog")]),
            ("Undeclared", [Some("fish"), Some("bird")]),
        ] {
            let Shape::Union(union) = shape_of(&shapes, name)? else {
                panic!("expected a union");
            };
            assert_eq!(
                union.tag.as_ref().map(|tag| tag.property.as_str()),
                Some("kind"),
                "{name}"
            );
            let tags: Vec<Option<&str>> = union
                .variants
                .iter()
                .map(|variant| variant.tag.as_deref())
                .collect();
            // The tag values are the constants the document wrote, not the component names.
            assert_eq!(tags, expected, "{name}");
        }
        // A variant used nowhere else gives the property up to the union that writes it.
        let Shape::Struct(cat) = shape_of(&shapes, "Cat")? else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = cat.fields.iter().map(|field| field.wire.as_str()).collect();
        assert_eq!(names, ["shared"]);
    }

    /// anthropic's tool list in miniature: a custom tool whose `type` may be left out or be
    /// `null`, a built-in tool named by one constant, and one that lists a dated name and an
    /// alias.
    /// Each payload still names one branch, so the union is read by its tag, and because the
    /// union could not write an alias or an absent tag back, the variants keep the property.
    #[test_util::test]
    fn a_branch_may_leave_its_tag_out_or_spell_it_more_than_one_way() {
        let tool = |kind: Value, required: Value| {
            json!({
                "type": "object",
                "required": required,
                "properties": {"type": kind, "name": {"type": "string"}},
                "additionalProperties": false,
            })
        };
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Custom": tool(
                json!({"anyOf": [{"type": "null"}, {"const": "custom", "type": "string"}]}),
                json!(["name"]),
            ),
            "Bash": tool(json!({"const": "bash", "type": "string"}), json!(["name", "type"])),
            "Search": tool(
                json!({"enum": ["search_20251119", "search"], "type": "string"}),
                json!(["name", "type"]),
            ),
            "Tool": {"oneOf": [
                {"$ref": "#/components/schemas/Custom"},
                {"$ref": "#/components/schemas/Bash"},
                {"$ref": "#/components/schemas/Search"},
            ]},
        })))?;
        assert!(
            !diagnostics
                .iter()
                .any(|found| found.class() == BreakageClass::WildUnion),
            "{diagnostics:?}"
        );
        let Shape::Union(union) = shape_of(&shapes, "Tool")? else {
            panic!("expected a union");
        };

        // The custom tool is what a payload without a tag, or with a `null` one, is.
        assert_eq!(
            union.tag,
            Some(Tag {
                property: "type".to_owned(),
                style: TagStyle::Carried,
                absent: Some(0),
                null: Some(0),
            })
        );
        // Each branch writes its first spelling and reads the rest.
        let spellings: Vec<(Option<&str>, &[String])> = union
            .variants
            .iter()
            .map(|variant| (variant.tag.as_deref(), variant.aliases.as_slice()))
            .collect();
        assert_eq!(
            spellings,
            [
                (Some("custom"), &[][..]),
                (Some("bash"), &[][..]),
                (Some("search_20251119"), &["search".to_owned()][..]),
            ]
        );
        // The variants keep the property, which holds the spelling or the absence.
        for name in ["Custom", "Bash", "Search"] {
            let Shape::Struct(tool) = shape_of(&shapes, name)? else {
                panic!("expected a struct");
            };
            assert!(
                tool.fields.iter().any(|field| field.wire == "type"),
                "{name}"
            );
        }

        // The one lookup reads a payload's tag member the way the generated decoder does.
        let named = |member: Option<Value>| {
            union
                .tagged_variant(member.as_ref())
                .and_then(|variant| variant.tag.clone())
        };
        assert_eq!(named(None), Some("custom".to_owned()));
        assert_eq!(named(Some(Value::Null)), Some("custom".to_owned()));
        assert_eq!(
            named(Some(json!("search"))),
            Some("search_20251119".to_owned())
        );
        assert_eq!(named(Some(json!("bash"))), Some("bash".to_owned()));
        assert_eq!(named(Some(json!(7))), None);
        assert_eq!(named(Some(json!("other"))), None);
    }

    /// A `null` tag names a branch only where every constraint on its tag admits it — its `enum`,
    /// its `type` and a union beside them — because a payload has to satisfy all of them.
    #[test_util::test]
    fn a_null_tag_names_only_a_branch_whose_whole_domain_admits_it() {
        let tool = |kind: Value| {
            json!({
                "type": "object",
                "required": ["type"],
                "properties": {"type": kind, "size": {"type": "integer"}},
            })
        };
        for (kind, null) in [
            (json!({"type": ["string", "null"], "enum": ["a"]}), None),
            (json!({"type": "string", "enum": ["a", null]}), None),
            (
                json!({"type": ["string", "null"], "enum": ["a", null]}),
                Some(0),
            ),
            (json!({"enum": ["a", null]}), Some(0)),
            // A union beside the type, or beside the list, has to admit `null` as well.
            (
                json!({"type": "string", "anyOf": [{"const": "a"}, {"type": "null"}]}),
                None,
            ),
            (
                json!({"enum": ["a", null], "anyOf": [{"const": "a"}, {"const": "z"}]}),
                None,
            ),
            (
                json!({"anyOf": [{"const": "a"}, {"type": "null"}]}),
                Some(0),
            ),
        ] {
            let (shapes, _) = shapes_of(with_schemas(json!({
                "A": tool(kind.clone()),
                "B": tool(json!({"const": "b"})),
                "Union": {"oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ]},
            })))?;
            let Shape::Union(union) = shape_of(&shapes, "Union")? else {
                panic!("expected a union");
            };
            let tag = union.tag.as_ref().ok_or_eyre("the constants are the tag")?;
            assert_eq!(tag.null, null, "{kind}");
        }
    }

    /// A union that has to carry its tag shares a variant with one that could consume it.
    /// Consuming would take the tag off the shared variant, where the first union still reads it,
    /// so both carry it, whichever of the two is settled first.
    #[test_util::test]
    fn a_union_forced_to_carry_its_tag_keeps_it_on_a_variant_another_union_shares() {
        let named = |constant: Value| {
            json!({
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": constant, "size": {"type": "integer"}},
            })
        };
        let refs = |names: [&str; 2]| json!({"oneOf": names.map(|name| json!({"$ref": format!("#/components/schemas/{name}")}))});
        // The forced union first in proposal order, then second.
        for (forced, consuming) in [("AForced", "BConsuming"), ("BForced", "AConsuming")] {
            let mut schemas = serde_json::Map::new();
            schemas.insert("Shared".to_owned(), named(json!({"const": "shared"})));
            schemas.insert(
                "Aliased".to_owned(),
                named(json!({"enum": ["aliased", "alias"]})),
            );
            schemas.insert("Plain".to_owned(), named(json!({"const": "plain"})));
            schemas.insert(forced.to_owned(), refs(["Shared", "Aliased"]));
            schemas.insert(consuming.to_owned(), refs(["Shared", "Plain"]));
            let (shapes, _) = shapes_of(with_schemas(Value::Object(schemas)))?;
            for name in [forced, consuming] {
                let Shape::Union(union) = shape_of(&shapes, name)? else {
                    panic!("{name}: expected a union");
                };
                assert_eq!(
                    union.tag.as_ref().map(|tag| tag.style),
                    Some(TagStyle::Carried),
                    "{name}"
                );
            }
            // The shared variant keeps the property both unions read.
            let Shape::Struct(shared) = shape_of(&shapes, "Shared")? else {
                panic!("expected a struct");
            };
            assert!(
                shared.fields.iter().any(|field| field.wire == "kind"),
                "{forced} first"
            );
        }
    }

    /// A tag that does not decide every payload is not read as one where it would guess.
    #[test_util::test]
    fn a_missing_tag_two_branches_could_take_or_an_open_tag_names_no_branch() {
        let tool = |kind: Value, required: Value| {
            json!({
                "type": "object",
                "required": required,
                "properties": {"type": kind, "size": {"type": "integer"}},
            })
        };
        let (shapes, _) = shapes_of(with_schemas(json!({
            "A": tool(json!({"enum": ["a", null]}), json!([])),
            "B": tool(json!({"enum": ["b", null]}), json!([])),
            "TwoOptional": {"oneOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/B"},
            ]},
            "Open": tool(json!({"anyOf": [{"const": "open"}, {"type": "string"}]}), json!(["type"])),
            "Closed": tool(json!({"const": "closed"}), json!(["type"])),
            "OpenTag": {"oneOf": [
                {"$ref": "#/components/schemas/Open"},
                {"$ref": "#/components/schemas/Closed"},
            ]},
            "Shared": tool(json!({"enum": ["c", "d"]}), json!(["type"])),
            "Clash": tool(json!({"const": "d"}), json!(["type"])),
            "SharedTag": {"oneOf": [
                {"$ref": "#/components/schemas/Shared"},
                {"$ref": "#/components/schemas/Clash"},
            ]},
        })))?;

        // Both branches may leave the tag out or set it to `null`, so neither names the branch.
        let Shape::Union(two) = shape_of(&shapes, "TwoOptional")? else {
            panic!("expected a union");
        };
        let tag = two
            .tag
            .as_ref()
            .ok_or_eyre("the constants are still the tag")?;
        assert_eq!((tag.absent, tag.null), (None, None));
        assert!(two.tagged_variant(None).is_none());

        // A branch whose tag takes any string does not name itself, so nothing is a tag.
        // A spelling two branches share does not either.
        for name in ["OpenTag", "SharedTag"] {
            assert!(
                !matches!(
                    shape_of(&shapes, name)?,
                    Shape::Union(Union { tag: Some(_), .. })
                ),
                "{name}"
            );
        }
    }

    #[test_util::test]
    fn a_variant_used_outside_its_union_keeps_its_tag_property_and_the_union_carries_the_tag() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            // The use that makes consuming a loss: here `kind` really is on the wire.
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        })))?;
        // Consuming would strip `kind` off `A` and lose it at `Holder.a`, so the tag stays on the
        // variants and the union dispatches without taking it. Nothing is lost anywhere, which is
        // why this settles silently.
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(either) = shape_of(&shapes, "Either")? else {
            panic!("expected a union");
        };
        assert_eq!(
            either.tag,
            Some(Tag {
                property: "kind".to_owned(),
                style: TagStyle::Carried,
                absent: None,
                null: None,
            })
        );
        let Shape::Struct(a) = shape_of(&shapes, "A")? else {
            panic!("expected a struct");
        };
        assert_eq!(a.fields.len(), 2);
    }

    #[test_util::test]
    fn a_union_whose_variant_lacks_the_tag_property_cannot_carry_it_and_degrades() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            // No `kind` here: replaying a payload into `B` would drop the tag on the way through,
            // and serializing the value again would lose it.
            "B": {"type": "object", "properties": {"shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        })))?;
        assert_eq!(shape_of(&shapes, "Either")?, &Shape::Any);
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::DiscriminatorEdgeCase)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(
            reported.detail().contains("outside this union"),
            "{reported}"
        );
        assert!(
            reported
                .detail()
                .contains("cannot carry the tag themselves"),
            "{reported}"
        );
    }

    #[test_util::test]
    fn a_borrowed_variant_without_the_tag_property_does_not_stop_consumption() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            // Borrowed — but `A` never declared `kind`, so there is nothing to take off it and
            // nothing this position could lose. The union writes the tag itself either way.
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(either) = shape_of(&shapes, "Either")? else {
            panic!("expected a union");
        };
        assert_eq!(
            either.tag,
            Some(Tag {
                property: "kind".to_owned(),
                style: TagStyle::Consumed,
                absent: None,
                null: None,
            })
        );
    }

    #[test_util::test]
    fn unions_sharing_a_variant_settle_on_carrying_the_tag_together() {
        // `Second` cannot consume, because `C` is used at `Holder.c` — so it carries. That makes
        // `A`'s payloads at `Second`'s positions carry `kind` on the wire, so `First` cannot strip
        // it either, and the settlement takes a second round to notice: `First` is asked again
        // once `Second`'s variant edges count as borrowed.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "C": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "First": {
                "oneOf": [{"$ref": "#/components/schemas/A"}, {"$ref": "#/components/schemas/B"}],
                "discriminator": {"propertyName": "kind"},
            },
            "Second": {
                "oneOf": [{"$ref": "#/components/schemas/A"}, {"$ref": "#/components/schemas/C"}],
                "discriminator": {"propertyName": "kind"},
            },
            "Holder": {"type": "object", "properties": {"c": {"$ref": "#/components/schemas/C"}}},
        })))?;
        // Both carried: every variant keeps `kind`, each value carries its own tag, and no union
        // takes anything off a type — so there is no multi-parent finding to report either.
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for union_name in ["First", "Second"] {
            let Shape::Union(union) = shape_of(&shapes, union_name)? else {
                panic!("expected a union");
            };
            assert_eq!(
                union.tag,
                Some(Tag {
                    property: "kind".to_owned(),
                    style: TagStyle::Carried,
                    absent: None,
                    null: None,
                }),
                "{union_name}"
            );
        }
        for variant_name in ["A", "B", "C"] {
            let Shape::Struct(variant) = shape_of(&shapes, variant_name)? else {
                panic!("expected a struct");
            };
            assert!(
                variant.fields.iter().any(|field| field.wire == "kind"),
                "{variant_name} lost `kind`: {variant:?}"
            );
        }
    }

    #[test_util::test]
    fn a_union_nothing_tells_apart_is_not_matched_against_whichever_branch_parses_first() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"shared": {"type": "string"}, "extra": {"type": "string"}}},
            // No discriminator, no required property either declares alone, no constants: an
            // untagged enum would read every `B` payload as an `A` and drop `extra`.
            "Either": {"oneOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/B"},
            ]},
        })))?;
        assert_eq!(shape_of(&shapes, "Either")?, &Shape::Any);
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::WildUnion)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(reported.detail().contains("no discriminator"), "{reported}");
    }

    #[test_util::test]
    fn a_branch_that_accepts_a_later_ones_payloads_is_not_told_apart_by_it() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Extended": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "id": {"type": "string"}},
                "required": ["id"],
            },
            // `Extended` requires something `Base` does not declare, which tells `Base`'s payloads
            // apart from `Extended` — the direction that does not matter, because `Base` is tried
            // first and is open, so it accepts `{"name": …, "id": …}` and drops the `id`.
            "Envelope": {"anyOf": [
                {"$ref": "#/components/schemas/Base"},
                {"$ref": "#/components/schemas/Extended"},
            ]},
        })))?;
        assert_eq!(shape_of(&shapes, "Envelope")?, &Shape::Any);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::WildUnion),
            "{diagnostics:?}"
        );
    }

    #[test_util::test]
    fn the_same_two_branches_in_the_other_order_need_no_tag() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Extended": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "id": {"type": "string"}},
                "required": ["id"],
            },
            "Envelope": {"anyOf": [
                {"$ref": "#/components/schemas/Extended"},
                {"$ref": "#/components/schemas/Base"},
            ]},
        })))?;
        // The pair is the same and the reading is sound: an `Extended` payload matches `Extended`,
        // and a `Base` payload has no `id` for `Extended` to find, so it falls through. Paired with
        // `a_branch_that_accepts_a_later_ones_payloads_is_not_told_apart_by_it`, this is why the
        // question has to be asked in declaration order — one pair of branches, two orders, and
        // only one of them sound, so a symmetric test cannot be right about both.
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(envelope) = shape_of(&shapes, "Envelope")? else {
            panic!("expected a union");
        };
        assert_eq!(envelope.tag, None);
        assert_eq!(envelope.variants.len(), 2);
    }

    #[test_util::test]
    fn one_property_holding_two_kinds_tells_the_branches_apart() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            // `workos` returns exactly this from one status. The branches share every property
            // name, which is what makes them look alike — but `message` is a string in one and a
            // list in the other, and deserializing a list into a `String` is an error, which is a
            // fall-through to the next variant rather than a wrong match.
            "Error": {"oneOf": [
                {
                    "type": "object", "required": ["message"],
                    "properties": {"message": {"type": "string"}},
                },
                {
                    "type": "object", "required": ["message", "error"],
                    "properties": {
                        "message": {"type": "array", "items": {"type": "string"}},
                        "error": {"type": "string"},
                    },
                },
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(error) = shape_of(&shapes, "Error")? else {
            panic!("expected a union");
        };
        assert_eq!(error.tag, None);
        assert_eq!(error.variants.len(), 2);
    }

    #[test_util::test]
    fn a_closed_branch_is_told_apart_by_what_a_later_one_requires() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "additionalProperties": false,
            },
            "Extended": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "id": {"type": "string"}},
                "required": ["id"],
            },
            "Envelope": {"anyOf": [
                {"$ref": "#/components/schemas/Base"},
                {"$ref": "#/components/schemas/Extended"},
            ]},
        })))?;
        // Closing `Base` is what makes the first order sound too: it is the one shape that turns a
        // payload down for carrying something extra, and every `Extended` payload carries `id`.
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(envelope) = shape_of(&shapes, "Envelope")? else {
            panic!("expected a union");
        };
        assert_eq!(envelope.tag, None);
    }

    #[test_util::test]
    fn a_branch_that_becomes_arbitrary_json_tells_nothing_apart() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            // A bare enumeration of objects narrows nothing progeny can type, so the branch is
            // `serde_json::Value` — which accepts every payload and would swallow `Payload`.
            "Loose": {"enum": [{"a": 1}, {"b": 2}]},
            "Payload": {
                "type": "object",
                "properties": {"k": {"type": "string"}},
                "required": ["k"],
            },
            "Either": {"anyOf": [
                {"$ref": "#/components/schemas/Loose"},
                {"$ref": "#/components/schemas/Payload"},
            ]},
        })))?;
        assert_eq!(shape_of(&shapes, "Either")?, &Shape::Any);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::WildUnion),
            "{diagnostics:?}"
        );
    }

    /// Whether classifying `schemas` reported a wild union, and typed `name` as arbitrary JSON.
    fn degrades_as_wild_union(schemas: Value, name: &str) -> eyre::Result<bool> {
        let (shapes, diagnostics) = shapes_of(with_schemas(schemas))?;
        let reported = diagnostics
            .iter()
            .any(|d| d.class() == crate::BreakageClass::WildUnion);
        Ok(reported && shape_of(&shapes, name)? == &Shape::Any)
    }

    /// Whether a schema degrades to arbitrary JSON because a branch would drop members declared
    /// beside its union, and the diagnostic names `members`.
    fn degrades_for_members_beside(
        schemas: Value,
        name: &str,
        members: &str,
    ) -> eyre::Result<bool> {
        let (shapes, diagnostics) = shapes_of(with_schemas(schemas))?;
        let reported = diagnostics.iter().any(|d| {
            d.class() == crate::BreakageClass::UnsupportedConstruct
                && d.detail().contains(&format!("declares {members} beside"))
        });
        Ok(reported && shape_of(&shapes, name)? == &Shape::Any)
    }

    /// The `ImageUserInput` of issue #26, with its union under `keyword` and `branch` as the
    /// second branch.
    fn image(keyword: &str, branch: &Value) -> Value {
        json!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {"type": "string", "enum": ["image"]},
                "detail": {"type": "string"},
            },
            keyword: [
                {"type": "object", "required": ["url"],
                 "properties": {"url": {"type": "string"}}},
                branch,
            ],
        })
    }

    /// A struct with a `#[serde(flatten)]` enum member, as `schemars` exports it, is that
    /// struct again: the members beside the union are its fields, and the union, a type of its
    /// own, reads the rest.
    #[test_util::test]
    fn members_beside_a_union_no_branch_names_are_a_struct_with_the_union_flattened() {
        let file_id = json!({"type": "object", "required": ["fileId"],
                             "properties": {"fileId": {"type": "string"}}});
        for keyword in ["anyOf", "oneOf"] {
            let (shapes, diagnostics) = shapes_of(with_schemas(
                json!({ "ImageUserInput": image(keyword, &file_id) }),
            ))?;
            let Shape::Struct(structure) = shape_of(&shapes, "ImageUserInput")? else {
                eyre::bail!("{keyword}: not a struct");
            };
            let wires: Vec<&str> = structure
                .fields
                .iter()
                .map(|field| field.wire.as_str())
                .collect();
            assert_eq!(wires, ["detail", "type"], "{keyword}");
            assert_eq!(structure.extra, Extra::Open, "{keyword}");
            // The union is keyed apart from the struct, and holds both branches.
            let Some(ShapeRef::Key(branches)) = &structure.flattened else {
                eyre::bail!("{keyword}: no flattened union");
            };
            assert!(branches.is_branches(), "{keyword}");
            let variants = match shapes.get(branches) {
                Some(Shape::Union(union)) => union.variants.len(),
                Some(Shape::Keyed(keyed)) => keyed.variants.len(),
                other => eyre::bail!("{keyword}: the flattened key is {other:?}"),
            };
            assert_eq!(variants, 2, "{keyword}");
            // Nothing is lost, so nothing is reported.
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.class() == BreakageClass::UnsupportedConstruct),
                "{keyword}: {diagnostics:?}"
            );
        }
    }

    /// A member both the struct and a branch could read, or one a closed struct would refuse,
    /// keeps the union degraded, because the flattened struct would give it to one reader.
    #[test_util::test]
    fn members_beside_a_union_a_branch_also_names_still_degrade_the_union() {
        // A branch that redeclares `detail` would never see it: the struct's field takes it first.
        let redeclares = json!({"type": "object", "required": ["fileId"], "properties": {
            "fileId": {"type": "string"}, "detail": {"type": "string"},
        }});
        // A branch that only requires `type` names it too.
        let requires = json!({"type": "object", "required": ["fileId", "type"],
                              "properties": {"fileId": {"type": "string"}}});
        for branch in [redeclares, requires] {
            assert!(
                degrades_for_members_beside(
                    json!({ "ImageUserInput": image("anyOf", &branch) }),
                    "ImageUserInput",
                    "`detail`, `type`",
                )?,
                "{branch}"
            );
        }
        // A closed struct says nothing of the branches' members, so they are its undeclared ones.
        let mut closed = image(
            "anyOf",
            &json!({"type": "object", "required": ["fileId"],
                    "properties": {"fileId": {"type": "string"}}}),
        );
        if let Some(object) = closed.as_object_mut() {
            object.insert("additionalProperties".to_owned(), json!(false));
        }
        assert!(degrades_for_members_beside(
            json!({ "ImageUserInput": closed }),
            "ImageUserInput",
            "`detail`, `type`",
        )?);
        // Patterns describe other members too, even ones that do not agree on a single type.
        let mut patterned = image(
            "anyOf",
            &json!({"type": "object", "required": ["fileId"],
                    "properties": {"fileId": {"type": "string"}}}),
        );
        if let Some(object) = patterned.as_object_mut() {
            object.insert(
                "patternProperties".to_owned(),
                json!({"^x-": {"type": "string"}, "^n-": {"type": "integer"}}),
            );
        }
        assert!(degrades_for_members_beside(
            json!({ "ImageUserInput": patterned }),
            "ImageUserInput",
            "`detail`, `type`",
        )?);
        // A branch that is not an object has no members to sit beside the struct's, and one that
        // may be `null` is not always one.
        for branch in [
            json!({"type": "string"}),
            json!({"type": ["object", "null"], "required": ["fileId"],
                   "properties": {"fileId": {"type": "string"}}}),
        ] {
            assert!(
                degrades_for_members_beside(
                    json!({ "ImageUserInput": image("anyOf", &branch) }),
                    "ImageUserInput",
                    "`detail`, `type`",
                )?,
                "{branch}"
            );
        }
        // One branch is an alias rather than a union, and stays a reported degradation, however
        // many times it is listed.
        let url = json!({"type": "object", "required": ["url"],
                         "properties": {"url": {"type": "string"}}});
        let mut single = image("anyOf", &url);
        if let Some(branches) = single.get_mut("anyOf").and_then(Value::as_array_mut) {
            branches.truncate(1);
        }
        let mut repeated = image("anyOf", &json!({"$ref": "#/components/schemas/Url"}));
        if let Some(first) = repeated.pointer_mut("/anyOf/0") {
            *first = json!({"$ref": "#/components/schemas/Url"});
        }
        for schema in [single, repeated] {
            assert!(
                degrades_for_members_beside(
                    json!({ "ImageUserInput": schema.clone(), "Url": url.clone() }),
                    "ImageUserInput",
                    "`detail`, `type`",
                )?,
                "{schema}"
            );
        }
    }

    /// A discriminator the branches inherit names its member as surely as a property, so a
    /// struct declaring that member beside them does not flatten them.
    #[test_util::test]
    fn an_inherited_discriminator_naming_a_member_beside_the_union_keeps_it_unflattened() {
        let branch = |own: &str| {
            json!({"allOf": [
                {"$ref": "#/components/schemas/Base"},
                {"type": "object", "required": ["x"], "properties": {
                    "x": {"type": "string"}, own: {"type": "string"},
                }},
            ]})
        };
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Base": {"discriminator": {"propertyName": "kind"}},
            "Holder": {
                "type": "object",
                "properties": {"kind": {"type": "string"}, "detail": {"type": "string"}},
                "oneOf": [branch("a"), branch("b")],
            },
        })))?;
        assert!(
            !matches!(
                shape_of(&shapes, "Holder")?,
                Shape::Struct(structure) if structure.flattened.is_some()
            ),
            "{:?}",
            shape_of(&shapes, "Holder")?
        );
    }

    /// A union a mapping names beside its schema's members is reported once, by the schema,
    /// though the struct flattens it as a type of its own.
    #[test_util::test]
    fn an_inheritance_union_flattened_beside_members_is_reported_once() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {
                "type": "object",
                "properties": {"detail": {"type": "string"}},
                "discriminator": {"propertyName": "kind", "mapping": {
                    "a": "#/components/schemas/A", "b": "#/components/schemas/B",
                }},
            },
            "A": {"type": "object", "required": ["kind", "a"], "properties": {
                "kind": {"type": "string"}, "a": {"type": "string"},
            }},
            "B": {"type": "object", "required": ["kind", "b"], "properties": {
                "kind": {"type": "string"}, "b": {"type": "string"},
            }},
        })))?;
        let repairs: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.class() == BreakageClass::InheritanceDiscriminator)
            .collect();
        assert_eq!(repairs.len(), 1, "{diagnostics:?}");
        assert!(repairs.iter().all(|d| d.related().is_empty()), "{repairs:?}");
        assert!(
            matches!(
                shape_of(&shapes, "Base")?,
                Shape::Struct(structure) if structure.flattened.is_some()
            ),
            "{:?}",
            shape_of(&shapes, "Base")?
        );
    }

    #[test_util::test]
    fn a_required_name_beside_a_union_is_a_member_a_branch_can_drop() {
        // A required name is a member too, even where no property declares it.
        assert!(degrades_for_members_beside(
            json!({
                "Named": {
                    "required": ["id"],
                    "anyOf": [
                        {"$ref": "#/components/schemas/A"},
                        {"type": "object", "properties": {"id": {"type": "string"}}},
                    ],
                },
                "A": {
                    "type": "object",
                    "required": ["a"],
                    "properties": {"a": {"type": "string"}},
                },
            }),
            "Named",
            "`id`",
        )?);
    }

    #[test_util::test]
    fn nullable_emulation_beside_members_drops_them_like_any_union() {
        // The one branch of nullable emulation drops them as surely as a union's branches do.
        assert!(degrades_for_members_beside(
            json!({
                "Maybe": {
                    "properties": {"note": {"type": "string"}},
                    "anyOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}],
                },
                "A": {"type": "object", "properties": {"a": {"type": "string"}}},
            }),
            "Maybe",
            "`note`",
        )?);
    }

    #[test_util::test]
    fn typed_extra_members_beside_a_union_need_a_branch_that_captures_them() {
        // Extra members the schema types beside the union are dropped by a struct that has no
        // capture for them.
        assert!(degrades_for_members_beside(
            json!({
                "Tagged": {
                    "additionalProperties": {"type": "string"},
                    "anyOf": [
                        {"$ref": "#/components/schemas/A"},
                        {"type": "object", "required": ["b"], "properties": {"b": {"type": "string"}}},
                    ],
                },
                "A": {
                    "type": "object",
                    "required": ["a"],
                    "properties": {"a": {"type": "string"}},
                },
            }),
            "Tagged",
            "the members its `additionalProperties` describes",
        )?);

        // An extra member can also land in a field the branch names and the schema does not.
        assert!(degrades_for_members_beside(
            json!({
                "Spread": {
                    "additionalProperties": {"type": "object", "properties": {
                        "x": {"type": "string"},
                    }},
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"payload": {
                                "type": "object", "properties": {"y": {"type": "string"}},
                            }},
                            "additionalProperties": {},
                        },
                        {"type": "null"},
                    ],
                },
            }),
            "Spread",
            "the members its `additionalProperties` describes",
        )?);

        // A pattern covers a name `properties` lists too, so its members may land in that field.
        assert!(degrades_for_members_beside(
            json!({
                "Patterned": {
                    "properties": {"payload": {}},
                    "patternProperties": {"^payload$": {"type": "object", "properties": {
                        "x": {"type": "string"},
                    }}},
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"payload": {
                                "type": "object", "properties": {"y": {"type": "string"}},
                            }},
                            "additionalProperties": {},
                        },
                        {"type": "null"},
                    ],
                },
            }),
            "Patterned",
            "the members its `patternProperties` describes",
        )?);
    }

    #[test_util::test]
    fn a_member_with_nested_members_survives_only_a_destination_that_keeps_them() {
        // A branch that redeclares an object member reads past the nested members only the schema
        // beside the union declares.
        assert!(degrades_for_members_beside(
            json!({
                "Nested": {
                    "properties": {"payload": {"type": "object", "properties": {
                        "id": {"type": "string"}, "note": {"type": "string"},
                    }}},
                    "anyOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}],
                },
                "A": {"type": "object", "properties": {"payload": {
                    "type": "object", "properties": {"id": {"type": "string"}},
                }}},
            }),
            "Nested",
            "`payload`",
        )?);

        // A member with nested members of its own survives neither a typed capture that may read
        // past them nor a subtyped base that only looks like a map.
        let payload = json!({"type": "object", "required": ["x"], "properties": {
            "x": {"type": "string"},
        }});
        for branch in [
            json!({"type": "object", "additionalProperties": {
                "type": "object", "properties": {"y": {"type": "string"}},
            }}),
            json!({"type": "object", "properties": {"payload": {
                "type": "object", "properties": {"y": {"type": "string"}},
            }}}),
            json!({"$ref": "#/components/schemas/Base"}),
            json!({"$ref": "#/components/schemas/Untyped"}),
        ] {
            assert!(
                degrades_for_members_beside(
                    json!({
                        "Holder": {
                            "properties": {"payload": payload.clone()},
                            "anyOf": [branch.clone(), {"type": "null"}],
                        },
                        "Base": {
                            "type": "object",
                            "discriminator": {"propertyName": "kind", "mapping": {
                                "c": "#/components/schemas/C", "d": "#/components/schemas/D",
                            }},
                        },
                        // A base becomes the union of its subtypes whether or not it says
                        // `type: object`.
                        "Untyped": {
                            "discriminator": {"propertyName": "kind", "mapping": {
                                "c": "#/components/schemas/C", "d": "#/components/schemas/D",
                            }},
                        },
                        "C": {"allOf": [{"$ref": "#/components/schemas/Base"}, {
                            "properties": {"c": {"type": "string"}},
                        }]},
                        "D": {"allOf": [{"$ref": "#/components/schemas/Base"}, {
                            "properties": {"d": {"type": "string"}},
                        }]},
                    }),
                    "Holder",
                    "`payload`",
                )?,
                "{branch}"
            );
        }

        // A member that may be an object or a string still declares the members an object has.
        assert!(degrades_for_members_beside(
            json!({
                "Either": {
                    "properties": {"payload": {"type": ["object", "string"], "properties": {
                        "x": {"type": "string"},
                    }}},
                    "anyOf": [
                        {"type": "object", "properties": {"payload": {
                            "type": "object", "properties": {"y": {"type": "string"}},
                        }}},
                        {"type": "null"},
                    ],
                },
            }),
            "Either",
            "`payload`",
        )?);
    }

    #[test_util::test]
    fn members_beside_a_union_that_every_branch_declares_keep_the_union() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            // Each branch redeclares the member, which is how a union with a shared field is
            // usually written.
            "Shared": {
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
            },
            "A": {"type": "object", "required": ["id", "a"], "properties": {
                "id": {"type": "string"}, "a": {"type": "integer"},
            }},
            "B": {"type": "object", "required": ["id", "b"], "properties": {
                "id": {"type": "string"}, "b": {"type": "boolean"},
            }},
            // A string carries no members, so it drops none.
            "Labelled": {
                "properties": {"note": {"type": "string"}},
                "anyOf": [{"enum": ["a"]}, {"enum": ["b"]}],
            },
            // The tag is read by the enum and written back by it.
            "Tagged": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"type": "string"}},
                "discriminator": {"propertyName": "kind"},
                "oneOf": [
                    {"$ref": "#/components/schemas/C"},
                    {"$ref": "#/components/schemas/D"},
                ],
            },
            "C": {"type": "object", "properties": {"c": {"type": "string"}}},
            "D": {"type": "object", "properties": {"d": {"type": "string"}}},
            // A map keeps every member, declared or not.
            "Open": {
                "properties": {"note": {"type": "string"}},
                "anyOf": [{"type": "object"}, {"type": "null"}],
            },
            // A member redeclared in the same shape is kept, though it is written out again.
            "Mapped": {
                "type": "object",
                "properties": {"metadata": {"type": "object", "additionalProperties": {
                    "type": "string",
                }}},
                "anyOf": [
                    {"type": "object", "required": ["e"], "properties": {
                        "e": {"type": "string"},
                        "metadata": {"type": "object", "additionalProperties": {"type": "string"}},
                    }},
                    {"type": "null"},
                ],
            },
            // A base that maps one subtype is not a union, so it stays the map it reads as.
            "Single": {
                "properties": {"note": {"type": "string"}},
                "anyOf": [{"$ref": "#/components/schemas/Lone"}, {"type": "null"}],
            },
            "Lone": {
                "type": "object",
                "discriminator": {"propertyName": "kind", "mapping": {
                    "c": "#/components/schemas/C",
                }},
            },
            // A member declared as arbitrary JSON keeps whatever it holds.
            "Loose": {
                "properties": {"payload": {"type": "object", "properties": {
                    "x": {"type": "string"},
                }}},
                "anyOf": [
                    {"type": "object", "properties": {"payload": {}}},
                    {"type": "null"},
                ],
            },
        })))?;
        assert!(matches!(shape_of(&shapes, "Shared")?, Shape::Union(_)));
        assert!(matches!(
            shape_of(&shapes, "Labelled")?,
            Shape::StringEnum(_)
        ));
        assert!(matches!(shape_of(&shapes, "Tagged")?, Shape::Union(_)));
        assert!(matches!(shape_of(&shapes, "Open")?, Shape::Optional(_)));
        assert!(matches!(shape_of(&shapes, "Loose")?, Shape::Optional(_)));
        assert!(matches!(shape_of(&shapes, "Mapped")?, Shape::Optional(_)));
        assert!(matches!(shape_of(&shapes, "Single")?, Shape::Optional(_)));
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.detail().contains("beside its `oneOf`/`anyOf`")),
            "{diagnostics:#?}"
        );
    }

    /// The variants of a keyed union as `(name, has payload)`, and whether it is open.
    fn keyed_of(shapes: &Shapes, name: &str) -> eyre::Result<(Vec<(String, bool)>, bool)> {
        let Shape::Keyed(keyed) = shape_of(shapes, name)? else {
            eyre::bail!(
                "{name} should be a keyed union, is {:?}",
                shape_of(shapes, name)?
            );
        };
        let variants = keyed
            .variants
            .iter()
            .map(|variant| (variant.name.clone(), variant.payload.is_some()))
            .collect();
        Ok((variants, keyed.open))
    }

    /// serde's externally tagged enum, as `schemars` exports it, read by its strings and keys.
    #[test_util::test]
    fn strings_and_single_member_objects_are_one_keyed_union() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "ReviewDecision": {"oneOf": [
                {"type": "string", "enum": ["approved"]},
                {"type": "object", "additionalProperties": false, "required": ["denied"],
                 "properties": {"denied": {"type": "object", "properties": {
                     "rejection": {"type": "string"},
                 }}}},
                {"type": "string", "enum": ["approved_for_session", "abort"]},
            ]},
            // A last branch that takes any other string or object is the open arm.
            "CodexErrorInfo": {"anyOf": [
                {"type": "string", "enum": ["overloaded"]},
                {"type": "object", "required": ["http"],
                 "properties": {"http": {"type": "integer"}}},
                {"type": ["string", "object"]},
            ]},
            // A nullable union is an `Option` of the keyed one.
            "Maybe": {"anyOf": [
                {"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
                {"type": "object", "required": ["b"], "properties": {"b": {"type": "null"}}},
                {"type": "null"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        let (variants, open) = keyed_of(&shapes, "ReviewDecision")?;
        assert_eq!(
            variants,
            [
                ("approved".to_owned(), false),
                ("denied".to_owned(), true),
                ("approved_for_session".to_owned(), false),
                ("abort".to_owned(), false),
            ]
        );
        assert!(!open);

        let (variants, open) = keyed_of(&shapes, "CodexErrorInfo")?;
        assert_eq!(variants.len(), 2);
        assert!(open);

        assert!(matches!(shape_of(&shapes, "Maybe")?, Shape::Optional(_)));
    }

    /// A union is read by its keys only where nothing about a branch would be lost by it.
    #[test_util::test]
    fn a_union_is_not_keyed_where_a_key_does_not_say_everything() {
        let member = |name: &str| json!({"type": "object", "required": [name], "properties": {name: {"type": "string"}}});
        let refused = [
            // A branch with a second member, even an optional one.
            json!({"oneOf": [
                {"type": "string", "enum": ["a"]},
                {"type": "object", "required": ["b"],
                 "properties": {"b": {"type": "string"}, "c": {"type": "string"}}},
            ]}),
            // Two branches under one key.
            json!({"oneOf": [member("b"), member("b"), {"type": "string", "enum": ["a"]}]}),
            // A string that is also a key.
            json!({"oneOf": [{"type": "string", "enum": ["b"]}, member("b")]}),
            // A key that is present but not required.
            json!({"oneOf": [
                {"type": "string", "enum": ["a"]},
                {"type": "object", "properties": {"b": {"const": "x"}}},
            ]}),
            // A catch-all that is not last.
            json!({"oneOf": [{"type": ["string", "object"]}, member("b")]}),
            // A last branch that accepts more than strings and objects.
            json!({"oneOf": [member("b"), {}]}),
            // Members declared beside the union.
            json!({"properties": {"c": {"type": "string"}}, "oneOf": [member("a"), member("b")]}),
            // A branch with something to report.
            json!({"oneOf": [member("a"), {"type": "string", "enum": ["b"], "maxLength": 3}]}),
            // A branch that also admits `null`.
            json!({"oneOf": [
                member("a"),
                {"type": ["object", "null"], "required": ["b"], "properties": {"b": {"type": "string"}}},
            ]}),
            // A member marked for one direction.
            json!({"oneOf": [
                member("a"),
                {"type": "object", "required": ["b"],
                 "properties": {"b": {"type": "string", "readOnly": true}}},
            ]}),
            // A catch-all for strings alone.
            json!({"oneOf": [member("a"), {"type": "string"}]}),
        ];
        for schema in refused {
            let (shapes, _) = shapes_of(with_schemas(json!({ "Either": schema.clone() })))?;
            assert!(
                !matches!(shape_of(&shapes, "Either")?, Shape::Keyed(_)),
                "{schema} should not be keyed"
            );
        }
    }

    /// serde's externally tagged enum, as `schemars` exports it.
    #[test_util::test]
    fn a_string_branch_beside_others_does_not_swallow_a_later_string() {
        // The first branch becomes an open enum that reads `"abort"` as its own unlisted value, so
        // the third branch would never be reached.
        // `Denied` has a member beside its key, so the union is not read by that key.
        assert!(degrades_as_wild_union(
            json!({
                "Denied": {
                    "type": "object", "required": ["denied"],
                    "properties": {"denied": {"type": "string"}, "note": {"type": "string"}},
                },
                "ReviewDecision": {"oneOf": [
                    {"type": "string", "enum": ["approved"]},
                    {"$ref": "#/components/schemas/Denied"},
                    {"type": "string", "enum": ["abort"]},
                ]},
            }),
            "ReviewDecision",
        )?);
    }

    #[test_util::test]
    fn a_constant_does_not_tell_two_objects_apart() {
        // `Plain` lacks `type`, so nothing reads it as a tag, and the open enum generated for
        // `LocalImage`'s `type` reads `localAudio` too.
        assert!(degrades_as_wild_union(
            json!({
                "LocalImage": {
                    "type": "object", "required": ["path", "type"],
                    "properties": {
                        "path": {"type": "string"},
                        "type": {"type": "string", "enum": ["localImage"]},
                    },
                },
                "LocalAudio": {
                    "type": "object", "required": ["path", "type"],
                    "properties": {
                        "path": {"type": "string"},
                        "type": {"type": "string", "enum": ["localAudio"]},
                    },
                },
                "Plain": {
                    "type": "object", "required": ["x"],
                    "properties": {"x": {"type": "string"}},
                },
                "Input": {"oneOf": [
                    {"$ref": "#/components/schemas/LocalImage"},
                    {"$ref": "#/components/schemas/LocalAudio"},
                    {"$ref": "#/components/schemas/Plain"},
                ]},
            }),
            "Input",
        )?);
    }

    #[test_util::test]
    fn a_nested_union_swallows_the_branches_after_it_but_not_the_ones_before() {
        let object = |name: &str| {
            json!({
                "type": "object", "required": [name],
                "properties": {name: {"type": "string"}},
            })
        };
        let inner = |first: &str, second: &str| {
            json!({"oneOf": [
                {"$ref": format!("#/components/schemas/{first}")},
                {"$ref": format!("#/components/schemas/{second}")},
            ]})
        };

        // `Inner`'s own fallback reads `{"d": …}`, so `D` after it is never reached.
        assert!(degrades_as_wild_union(
            json!({
                "B": object("b"), "C": object("c"), "D": object("d"),
                "Inner": inner("B", "C"),
                "Outer": inner("Inner", "D"),
            }),
            "Outer",
        )?);

        // Last, the same fallback only runs once `D` has turned the payload down.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "B": object("b"), "C": object("c"), "D": object("d"),
            "Inner": inner("B", "C"),
            "Outer": inner("D", "Inner"),
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(outer) = shape_of(&shapes, "Outer")? else {
            panic!("expected a union");
        };
        assert_eq!(outer.variants.len(), 2);
    }

    #[test_util::test]
    fn a_nested_union_of_strings_is_one_string_branch() {
        // `Mode` becomes one string enum, which turns every object down, so it can sit first.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Mode": {"anyOf": [{"const": "fast"}, {"const": "slow"}]},
            "Custom": {
                "type": "object", "required": ["speed"],
                "properties": {"speed": {"type": "number"}},
            },
            "Setting": {"oneOf": [
                {"$ref": "#/components/schemas/Mode"},
                {"$ref": "#/components/schemas/Custom"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Setting")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn string_branches_merge_into_one_open_enum() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Effort": {"oneOf": [
                {"type": "string", "enum": ["auto", "concise", "detailed"]},
                {"type": "string", "enum": ["none"]},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let values = ["auto", "concise", "detailed", "none"].map(ToOwned::to_owned);
        assert_eq!(
            shape_of(&shapes, "Effort")?,
            &Shape::StringEnum(values.to_vec())
        );
    }

    #[test_util::test]
    fn listed_strings_before_any_string_are_not_matched_by_shape() {
        // The open enum reads `"custom"` as an unlisted value of its own, where the document means
        // the plain string after it.
        assert!(degrades_as_wild_union(
            json!({"Model": {"anyOf": [
                {"type": "string", "enum": ["small", "large"]},
                {"type": "string"},
            ]}}),
            "Model",
        )?);

        // After any string, the listed ones are payloads the plain string already admits, so the
        // document itself does not say which branch they are.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({"Model": {"anyOf": [
            {"type": "string"},
            {"type": "string", "enum": ["small", "large"]},
        ]}})))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Model")?, Shape::Union(_)));

        // Behind a plain string no string reaches the constants, so they are never compared with
        // one another. `anthropic` names its models this way.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({"Model": {"anyOf": [
            {"type": "string"},
            {"const": "small"},
            {"const": "large"},
        ]}})))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Model")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn a_constrained_string_branch_is_not_folded_out_of_sight() {
        // Folded into one enum, the first branch would never be classified and its `pattern` never
        // reported, so the union is matched by shape instead, where the open enum it becomes
        // would read `"b"`.
        assert!(degrades_as_wild_union(
            json!({"Code": {"oneOf": [
                {"type": "string", "enum": ["aa", "ab"], "pattern": "^aa$"},
                {"type": "string", "enum": ["b"]},
            ]}}),
            "Code",
        )?);
    }

    #[test_util::test]
    fn an_optional_wrapper_does_not_hide_a_nested_unions_fallback() {
        let object = |name: &str| {
            json!({
                "type": "object", "required": [name],
                "properties": {name: {"type": "string"}},
            })
        };
        // `Maybe` is an `Option<Inner>`, and `Inner`'s fallback reads `{"d": …}` before `D` is
        // tried, however many wrappers stand between them.
        assert!(degrades_as_wild_union(
            json!({
                "B": object("b"), "C": object("c"), "D": object("d"),
                "Inner": {"oneOf": [
                    {"$ref": "#/components/schemas/B"},
                    {"$ref": "#/components/schemas/C"},
                ]},
                "Maybe": {"anyOf": [{"type": "null"}, {"$ref": "#/components/schemas/Inner"}]},
                "Outer": {"oneOf": [
                    {"$ref": "#/components/schemas/Maybe"},
                    {"$ref": "#/components/schemas/D"},
                ]},
            }),
            "Outer",
        )?);
    }

    #[test_util::test]
    fn a_nested_union_of_strings_is_one_branch_wherever_it_sits() {
        // `Mode` is one string enum, so its two constants are never compared with each other.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Mode": {"anyOf": [{"const": "fast"}, {"const": "slow"}]},
            "Setting": {"oneOf": [
                {"type": "integer"},
                {"$ref": "#/components/schemas/Mode"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Setting")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn only_a_member_the_struct_insists_on_tells_objects_apart() {
        // A second member keeps each branch from being read by its one key.
        let d = json!({
            "type": "object", "required": ["d"],
            "properties": {"d": {"type": "string"}, "y": {"type": "string"}},
        });
        // Required but nullable, `a` is an `Option` that reads a missing member as `None`, so the
        // first branch reads `{"d": …}` and drops `d`.
        assert!(degrades_as_wild_union(
            json!({
                "A": {
                    "type": "object", "required": ["a"],
                    "properties": {"a": {"type": ["string", "null"]}, "x": {"type": "string"}},
                },
                "D": d,
                "Either": {"oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/D"},
                ]},
            }),
            "Either",
        )?);

        // `p` is a string in one and a number in the other, but a payload of the second may leave
        // it out, and then nothing shows the first one the other kind.
        assert!(degrades_as_wild_union(
            json!({
                "A": {"type": "object", "properties": {"p": {"type": "string"}}},
                "B": {
                    "type": "object", "required": ["b"],
                    "properties": {"p": {"type": "number"}, "b": {"type": "string"}},
                },
                "Either": {"oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ]},
            }),
            "Either",
        )?);
    }

    #[test_util::test]
    fn a_nullable_member_behind_an_alias_does_not_tell_objects_apart() {
        // `P` is a union of one nullable branch, so `p` is still an `Option` that reads a missing
        // member as `None`, and the first branch reads `{"d": …}`.
        assert!(degrades_as_wild_union(
            json!({
                "P": {"anyOf": [{"type": ["string", "null"]}]},
                // A second member keeps each branch from being read by its one key.
                "A": {
                    "type": "object", "required": ["p"],
                    "properties": {"p": {"$ref": "#/components/schemas/P"}, "x": {"type": "string"}},
                },
                "D": {
                    "type": "object", "required": ["d"],
                    "properties": {"d": {"type": "string"}, "y": {"type": "string"}},
                },
                "Either": {"oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/D"},
                ]},
            }),
            "Either",
        )?);
    }

    #[test_util::test]
    fn a_nested_union_is_compared_as_the_type_it_becomes() {
        let object = |name: &str| {
            json!({
                "type": "object", "required": [name],
                "properties": {name: {"type": "string"}},
            })
        };
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "A": object("a"), "B": object("b"),
            // `A` again inside `Inner` is the same branch, which reads a payload the same way
            // whichever of the two matches it.
            "Inner": {"anyOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/B"},
            ]},
            "Repeats": {"anyOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/Inner"},
            ]},
            // A union of nothing but `null` is `null`, which no object payload fits.
            "Nothing": {"anyOf": [{"type": "null"}, {"type": "null"}]},
            "Null": {"anyOf": [
                {"$ref": "#/components/schemas/Nothing"},
                {"$ref": "#/components/schemas/A"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Repeats")?, Shape::Union(_)));
        assert!(matches!(shape_of(&shapes, "Null")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn a_string_branch_with_its_own_diagnostic_is_not_folded() {
        // Folded, the vendor `type` would never be reported, so the branches stay separate and the
        // union is matched by shape, where the first open enum would read `"c"`.
        let (_, diagnostics) = shapes_of(with_schemas(json!({"Code": {"oneOf": [
            {"type": "vendor-string", "enum": ["a", "b"]},
            {"enum": ["c"]},
        ]}})))?;
        assert!(
            !diagnostics.is_empty(),
            "a folded branch hid its diagnostic: {diagnostics:?}"
        );
    }

    #[test_util::test]
    fn an_open_tuple_is_arbitrary_json_and_tells_nothing_apart() {
        // A prefix followed by an `items` that admits values degrades to `serde_json::Value`,
        // which reads `{"d": …}` before `D` is tried.
        assert!(degrades_as_wild_union(
            json!({
                "D": {
                    "type": "object", "required": ["d"],
                    "properties": {"d": {"type": "string"}},
                },
                "Either": {"oneOf": [
                    {
                        "type": "array",
                        "prefixItems": [{"type": "string"}],
                        "items": {"type": "number"},
                    },
                    {"$ref": "#/components/schemas/D"},
                ]},
            }),
            "Either",
        )?);

        // Beside `type: string` the same keywords make no array at all, so the branch is the string
        // it says it is, which turns every object down.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "D": {
                "type": "object", "required": ["d"],
                "properties": {"d": {"type": "string"}},
            },
            "Either": {"oneOf": [
                {"type": "string", "prefixItems": [{"type": "string"}], "items": true},
                {"$ref": "#/components/schemas/D"},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Either")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn a_member_the_earlier_struct_insists_on_tells_objects_apart_by_kind() {
        // The later `p` may be `null`, but the earlier one is a required `String`, which turns down
        // an integer, a `null` and an absent member alike.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({"Either": {"oneOf": [
            {
                "type": "object", "required": ["p"],
                "properties": {"p": {"type": "string"}},
            },
            {
                "type": "object", "required": ["p", "x"],
                "properties": {"p": {"type": ["integer", "null"]}, "x": {"type": "integer"}},
            },
        ]}})))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(shape_of(&shapes, "Either")?, Shape::Union(_)));
    }

    #[test_util::test]
    fn a_listed_number_does_not_swallow_a_later_number() {
        // A numeric enumeration is the plain integer, so the first branch reads `2` as well.
        assert!(degrades_as_wild_union(
            json!({"Code": {"oneOf": [{"const": 1}, {"const": 2}]}}),
            "Code",
        )?);
    }

    #[test_util::test]
    fn a_bare_numeric_enum_is_a_number_rather_than_arbitrary_json() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Code": {"enum": [1, 2, 3]},
            "Ratio": {"enum": [0.5, 1.5]},
            "Flag": {"enum": [true, false]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // `type` is not the only way a document names a kind, and reading these as arbitrary JSON
        // lost the type *and* gave a union branch that swallows its siblings.
        assert_eq!(
            shape_of(&shapes, "Code")?,
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
        assert_eq!(shape_of(&shapes, "Ratio")?, &Shape::Scalar(Scalar::Number));
        assert_eq!(shape_of(&shapes, "Flag")?, &Shape::Scalar(Scalar::Bool));
    }

    #[test_util::test]
    fn an_enum_that_repeats_a_value_gives_one_variant_per_distinct_value() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            // Not adjacent, which is the whole reason `Vec::dedup` missed it: two variants renamed
            // to `"a"` gave serde a second one it can never produce or report.
            "Repeats": {"type": "string", "enum": ["a", "b", "a"]},
        })))?;
        assert_eq!(
            shape_of(&shapes, "Repeats")?,
            &Shape::StringEnum(vec!["a".to_owned(), "b".to_owned()])
        );
    }

    #[test_util::test]
    fn branches_of_different_kinds_still_need_no_tag() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Either": {"anyOf": [
                {"type": "string"},
                {"type": "integer"},
                {"type": "array", "items": {"type": "string"}},
            ]},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(either) = shape_of(&shapes, "Either")? else {
            panic!("expected a union");
        };
        // 754 of the corpus's `anyOf`s are this. Nothing is lost by matching them structurally,
        // and degrading them would be a large loss for a hazard that does not apply.
        assert_eq!(either.variants.len(), 3);
        assert_eq!(either.tag, None);
    }

    #[test_util::test]
    fn an_unmapped_variant_is_refused_rather_than_named_after_its_rust_type() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Holder": {
                "type": "object",
                "properties": {
                    "either": {
                        "oneOf": [
                            {"type": "object", "properties": {"kind": {"type": "string"}, "a": {"type": "string"}}},
                            {"type": "object", "properties": {"kind": {"type": "string"}, "b": {"type": "string"}}},
                        ],
                        "discriminator": {"propertyName": "kind"},
                    },
                },
            },
        })))?;
        let Shape::Struct(holder) = shape_of(&shapes, "Holder")? else {
            panic!("expected a struct");
        };
        let ShapeRef::Key(either) = &holder.fields[0].shape else {
            panic!("expected a key");
        };
        // Inline branches have no component name, and the mapping names nothing either, so there
        // is no string the tag would hold that the document actually stated.
        assert_eq!(shapes.get(either), Some(&Shape::Any));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::DiscriminatorEdgeCase),
            "{diagnostics:?}"
        );
    }

    #[test_util::test]
    fn a_variant_that_is_itself_a_response_body_keeps_its_tag() {
        let document = json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {
                "/file": {"get": {
                    "operationId": "getFile",
                    "responses": {"200": {
                        "description": "the file itself, where `kind` really is on the wire",
                        "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/FromFile"},
                        }},
                    }},
                }},
            },
            "components": {"schemas": {
                "FromFile": {
                    "type": "object", "required": ["kind", "location"],
                    "properties": {"kind": {"type": "string"}, "location": {"type": "string"}},
                },
                "FromUrl": {
                    "type": "object", "required": ["kind", "location"],
                    "properties": {"kind": {"type": "string"}, "location": {"type": "string"}},
                },
                "Source": {
                    "oneOf": [
                        {"$ref": "#/components/schemas/FromFile"},
                        {"$ref": "#/components/schemas/FromUrl"},
                    ],
                    "discriminator": {"propertyName": "kind"},
                },
            }},
        });
        let (shapes, diagnostics) = shapes_of(document)?;
        // The refusal of consumption comes from the half of the safety condition that has no edge
        // to be found by: an API-surface position points at no shape, so a walk over shapes alone
        // never sees that `FromFile` is a `200` body. Stripping `kind` there loses it on the
        // wire — so the tag stays on the variants and the union carries it instead.
        let Shape::Struct(from_file) = shape_of(&shapes, "FromFile")? else {
            panic!("expected a struct");
        };
        assert!(
            from_file.fields.iter().any(|field| field.wire == "kind"),
            "the response body kept `kind`: {from_file:?}"
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(source) = shape_of(&shapes, "Source")? else {
            panic!("expected a union");
        };
        assert_eq!(
            source.tag,
            Some(Tag {
                property: "kind".to_owned(),
                style: TagStyle::Carried,
                absent: None,
                null: None,
            })
        );
    }

    #[test_util::test]
    fn a_variant_only_a_declared_component_points_at_is_not_on_the_wire() {
        // The same document, with the `200` moved into a `components.responses` entry that no
        // operation references. Being *declared* is not being on the wire — the same reason a
        // `components.schemas` entry does not count — so the tag is affordable here and the union
        // keeps its type instead of degrading to arbitrary JSON.
        let document = json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {
                "/source": {"get": {
                    "operationId": "getSource",
                    "responses": {"200": {
                        "description": "the union itself",
                        "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/Source"},
                        }},
                    }},
                }},
            },
            "components": {
                "responses": {
                    "TheFile": {
                        "description": "declared and never used",
                        "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/FromFile"},
                        }},
                    },
                },
                "schemas": {
                    "FromFile": {
                        "type": "object", "required": ["kind", "location"],
                        "properties": {"kind": {"type": "string"}, "location": {"type": "string"}},
                    },
                    "FromUrl": {
                        "type": "object", "required": ["kind", "location"],
                        "properties": {"kind": {"type": "string"}, "location": {"type": "string"}},
                    },
                    "Source": {
                        "oneOf": [
                            {"$ref": "#/components/schemas/FromFile"},
                            {"$ref": "#/components/schemas/FromUrl"},
                        ],
                        "discriminator": {"propertyName": "kind"},
                    },
                },
            },
        });
        let (shapes, _) = shapes_of(document)?;
        let Shape::Union(union) = shape_of(&shapes, "Source")? else {
            panic!(
                "the union should have kept its type: {:?}",
                shape_of(&shapes, "Source")?
            );
        };
        assert_eq!(
            union.tag,
            Some(Tag {
                property: "kind".to_owned(),
                style: TagStyle::Consumed,
                absent: None,
                null: None,
            })
        );
        // And the variant gave the property up, because nothing else carries it.
        let Shape::Struct(from_file) = shape_of(&shapes, "FromFile")? else {
            panic!("expected a struct");
        };
        assert!(
            !from_file.fields.iter().any(|field| field.wire == "kind"),
            "{from_file:?}"
        );
    }

    #[test_util::test]
    fn an_explicit_mapping_names_the_variants_the_document_chose() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {
                    "propertyName": "kind",
                    // Both spellings OpenAPI allows, side by side.
                    "mapping": {"first": "#/components/schemas/A", "second": "B"},
                },
            },
        })))?;
        let Shape::Union(either) = shape_of(&shapes, "Either")? else {
            panic!("expected a union");
        };
        let tags: Vec<Option<&str>> = either
            .variants
            .iter()
            .map(|variant| variant.tag.as_deref())
            .collect();
        assert_eq!(tags, [Some("first"), Some("second")]);
    }

    #[test_util::test]
    fn a_draft_04_tuple_is_a_tuple_once_normalization_has_run() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pair": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}]},
        })))?;
        let Shape::Tuple { items } = shape_of(&shapes, "Pair")? else {
            panic!("expected a tuple");
        };
        assert_eq!(items.len(), 2);
    }

    /// `additionalItems: false` — normalized to `items: false` — forbids a tail, which is
    /// exactly what a fixed-arity tuple expresses.
    #[test_util::test]
    fn a_tuple_whose_tail_admits_nothing_stays_a_tuple() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Pair": {"type": "array",
                     "items": [{"type": "string"}, {"type": "integer"}],
                     "additionalItems": false},
        })))?;
        let Shape::Tuple { items } = shape_of(&shapes, "Pair")? else {
            panic!("expected a tuple");
        };
        assert_eq!(items.len(), 2);
        assert!(
            !diagnostics
                .iter()
                .any(|found| found.class() == BreakageClass::UnsupportedConstruct),
            "{diagnostics:?}"
        );
    }

    /// A prefix followed by an `items` that admits values allows instances longer than the
    /// prefix, and a generated `(A, B)` would refuse them — wrong about the wire, and until this
    /// diagnostic existed, silently so: the tail was carried in a field nothing ever lowered.
    #[test_util::test]
    fn a_tuple_whose_tail_admits_values_degrades_loudly() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Row": {"type": "array",
                    "prefixItems": [{"type": "string"}, {"type": "integer"}],
                    "items": {"type": "number"}},
        })))?;
        assert!(matches!(shape_of(&shapes, "Row")?, Shape::Any));
        let found: Vec<_> = diagnostics
            .iter()
            .filter(|found| found.class() == BreakageClass::UnsupportedConstruct)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].detail().contains("prefixItems"), "{found:#?}");
    }

    #[test_util::test]
    fn a_fixed_length_array_is_told_apart_from_a_list() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "List": {"type": "array", "items": {"type": "number"}, "minItems": 1},
        })))?;
        assert!(matches!(
            shape_of(&shapes, "Point")?,
            Shape::FixedArray { len: 3, .. }
        ));
        assert!(matches!(shape_of(&shapes, "List")?, Shape::Array { .. }));
        let reported = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.detail().contains("`minItems`"))
            .ok_or_eyre("the list's untyped lower bound should be reported")?;
        assert_eq!(reported.occurrences().get(), 1);
        assert!(!reported.detail().contains("`maxItems`"), "{reported}");
    }

    #[test_util::test]
    fn a_validating_keyword_does_not_narrow_the_type_but_is_reported() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Odd": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "not": {"required": ["b"]},
            },
        })))?;
        // The struct survives: `not` narrows validity, and dropping the type would lose more than
        // ignoring the keyword does.
        assert!(matches!(shape_of(&shapes, "Odd")?, Shape::Struct(_)));
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::UnsupportedConstruct)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(reported.detail().contains("`not`"), "{reported}");
    }

    #[test_util::test]
    fn every_untyped_value_constraint_is_reported() {
        let (_, diagnostics) = shapes_of(with_schemas(json!({
            "Multiple": {"type": "number", "multipleOf": 2},
            "Maximum": {"type": "number", "maximum": 10},
            "ExclusiveMaximum": {"type": "number", "exclusiveMaximum": 10},
            "Minimum": {"type": "number", "minimum": -10},
            "ExclusiveMinimum": {"type": "number", "exclusiveMinimum": -10},
            "MaxLength": {"type": "string", "maxLength": 10},
            "MinLengthA": {"type": "string", "minLength": 1},
            "MinLengthB": {"type": "string", "minLength": 2},
            "Pattern": {"type": "string", "pattern": "^[a-z]+$"},
            "MaxItems": {"type": "array", "items": true, "maxItems": 10},
            "MinItems": {"type": "array", "items": true, "minItems": 1},
            "UniqueItems": {"type": "array", "items": true, "uniqueItems": true},
            "MaxContains": {
                "type": "array",
                "contains": {"type": "string"},
                "maxContains": 2
            },
            "MinContains": {
                "type": "array",
                "contains": {"type": "string"},
                "minContains": 1
            },
            "MaxProperties": {"type": "object", "maxProperties": 2},
            "MinProperties": {"type": "object", "minProperties": 1},
        })))?;
        for keyword in [
            "multipleOf",
            "maximum",
            "exclusiveMaximum",
            "minimum",
            "exclusiveMinimum",
            "maxLength",
            "minLength",
            "pattern",
            "maxItems",
            "minItems",
            "uniqueItems",
            "maxContains",
            "minContains",
            "maxProperties",
            "minProperties",
        ] {
            let named = format!("`{keyword}`");
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.detail().contains(&named)),
                "{keyword} was silent: {diagnostics:#?}"
            );
        }
        let min_length = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.detail().starts_with("`minLength` narrow"))
            .ok_or_eyre("the repeated constraint should produce a diagnostic")?;
        assert_eq!(min_length.occurrences().get(), 2);
    }

    #[test_util::test]
    fn a_value_constraint_beside_a_reference_cannot_vanish() {
        let (_, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {"type": "string"},
            "Constrained": {
                "$ref": "#/components/schemas/Base",
                "pattern": "^[a-z]+$"
            },
        })))?;
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("`pattern`")),
            "{diagnostics:#?}"
        );
    }

    #[test_util::test]
    fn the_swagger_era_file_type_is_repaired_into_a_binary_string() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Upload": {"type": "file"},
        })))?;
        assert_eq!(
            shape_of(&shapes, "Upload")?,
            &Shape::Format(super::Format::Binary)
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].action(), crate::Action::Repair);
    }

    /// A lower bound of zero is what an unsigned integer expresses, so it is not a degradation.
    #[test_util::test]
    fn a_zero_lower_bound_on_an_unsigned_integer_is_not_reported() {
        struct Case {
            /// Text, because `json!` would read `1e-400` as `0.0` before progeny sees it.
            schema: &'static str,
            shape: Option<Shape>,
            bound_reported: bool,
        }
        let unsigned = || Shape::Scalar(Scalar::Integer { signed: false });
        let case = |schema, shape, bound_reported| Case {
            schema,
            shape,
            bound_reported,
        };
        let cases = [
            // Expressed by the unsigned integer
            case(
                r#"{"type": "integer", "minimum": 0}"#,
                Some(unsigned()),
                false,
            ),
            case(
                r#"{"type": ["integer", "null"], "format": "uint32", "minimum": 0}"#,
                Some(Shape::Optional(ShapeRef::Inline(Box::new(unsigned())))),
                false,
            ),
            case(
                r#"{"allOf": [{"type": "integer"}, {"minimum": 0}, {"minimum": -3}]}"#,
                Some(unsigned()),
                false,
            ),
            case(r#"{"enum": [0, 1], "minimum": 0}"#, Some(unsigned()), false),
            // Narrower than the unsigned integer
            case(
                r#"{"type": "integer", "minimum": 1}"#,
                Some(unsigned()),
                true,
            ),
            case(
                r#"{"type": "integer", "exclusiveMinimum": 0}"#,
                Some(unsigned()),
                true,
            ),
            case(
                r#"{"allOf": [{"type": "integer", "minimum": 0}, {"minimum": 2}]}"#,
                Some(unsigned()),
                true,
            ),
            case(
                r#"{"type": "integer", "minimum": 1e-400}"#,
                Some(unsigned()),
                true,
            ),
            // Not an unsigned integer at all
            case(r#"{"type": "number", "minimum": 0}"#, None, true),
            case(
                r#"{"type": "integer", "minimum": 0, "anyOf": [{"type": "integer"}]}"#,
                None,
                true,
            ),
        ];
        for Case {
            schema,
            shape,
            bound_reported,
        } in cases
        {
            let parsed: Value = serde_json::from_str(schema)?;
            let (shapes, diagnostics) = shapes_of(with_schemas(json!({ "Bounded": parsed })))?;
            let reported = diagnostics
                .iter()
                .any(|found| found.class() == BreakageClass::UnsupportedConstruct);
            assert_eq!(reported, bound_reported, "{schema}: {diagnostics:#?}");
            if let Some(shape) = shape {
                assert_eq!(shape_of(&shapes, "Bounded")?, &shape, "{schema}");
            }
        }
    }

    #[test_util::test]
    fn integers_take_their_sign_from_bounds_and_never_their_width() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Count": {"type": "integer", "minimum": 0, "maximum": 100},
            "Offset": {"type": "integer", "minimum": -5},
            "Plain": {"type": "integer"},
        })))?;
        assert_eq!(
            shape_of(&shapes, "Count")?,
            &Shape::Scalar(Scalar::Integer { signed: false })
        );
        assert_eq!(
            shape_of(&shapes, "Offset")?,
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
        assert_eq!(
            shape_of(&shapes, "Plain")?,
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
    }
}
