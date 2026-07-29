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

/// The set of schemas a shape is the classification of.
///
/// Sorted and deduplicated, so that two documents writing the same `allOf` in different orders
/// produce one key and therefore one type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ShapeKey(Vec<SchemaId>);

impl ShapeKey {
    fn new(mut parts: Vec<SchemaId>) -> Self {
        parts.sort_unstable();
        parts.dedup();
        Self(parts)
    }

    pub(crate) fn parts(&self) -> &[SchemaId] {
        &self.0
    }

    /// The schema that gives this shape its address: the first part, in id order.
    ///
    /// Ids are assigned in document order, so this is the earliest-written schema of the set — a
    /// deterministic choice that does not depend on which position reached the key first.
    pub(crate) fn anchor(&self) -> Option<SchemaId> {
        self.0.first().copied()
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
/// Closed and short on purpose. Every other `format` — `uri`, `email`, `hostname`, `ipv4` — maps
/// to `String` in v1, so recognizing it would add a variant that changes nothing. `format` is an
/// annotation rather than an assertion, and the model keeps the spelling either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Format {
    DateTime,
    Date,
    Time,
    Uuid,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Union {
    pub(crate) variants: Vec<Variant>,
    /// The property that says which variant a payload is, for a union progeny tags internally.
    ///
    /// `None` is the common case and means the variants are told apart by their shape. A union is
    /// only tagged when structural matching would be *unsound* — see [`super::discriminate`] —
    /// because tagging costs the variant types the tag property itself, and paying that when the
    /// shapes already distinguish the variants would be a loss for nothing.
    pub(crate) tag: Option<String>,
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
    for site in roots {
        let key = merge::key_of(resolved, site.id);
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
        let shape = classify::key(resolved, &key, ctx);
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
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{Extra, Scalar, Shape, ShapeRef, Shapes, classify};
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
            "Cat": {"type": "object", "required": ["meow"], "properties": {"meow": {"type": "string"}}},
            "Dog": {"type": "object", "required": ["woof"], "properties": {"woof": {"type": "string"}}},
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
        assert_eq!(either.tag.as_deref(), Some("kind"));
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

    #[test_util::test]
    fn a_constant_tag_beats_the_discriminator_because_it_costs_the_variants_nothing() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Cat": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"const": "cat"}, "shared": {"type": "string"}},
            },
            "Dog": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"const": "dog"}, "shared": {"type": "string"}},
            },
            "Animal": {
                "oneOf": [
                    {"$ref": "#/components/schemas/Cat"},
                    {"$ref": "#/components/schemas/Dog"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(animal) = shape_of(&shapes, "Animal")? else {
            panic!("expected a union");
        };
        // The constants already tell a payload apart, so nothing is tagged and nothing is taken
        // away: `Cat` keeps `kind`, and is still usable outside the union.
        assert_eq!(animal.tag, None);
        let Shape::Struct(cat) = shape_of(&shapes, "Cat")? else {
            panic!("expected a struct");
        };
        let names: Vec<&str> = cat.fields.iter().map(|field| field.wire.as_str()).collect();
        assert_eq!(names, ["kind", "shared"]);
    }

    #[test_util::test]
    fn a_variant_used_outside_its_union_keeps_its_tag_property_and_the_union_degrades() {
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
            // The use that makes stripping a loss: here `kind` really is on the wire.
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        })))?;
        assert_eq!(shape_of(&shapes, "Either")?, &Shape::Any);
        let Shape::Struct(a) = shape_of(&shapes, "A")? else {
            panic!("expected a struct");
        };
        assert_eq!(a.fields.len(), 2);
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::DiscriminatorEdgeCase)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(
            reported.detail().contains("outside this union"),
            "{reported}"
        );
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
        // The refusal the union table always specified, from the half of the safety condition that
        // has no edge to be found by: an API-surface position points at no shape, so a walk over
        // shapes alone never sees that `FromFile` is a `200` body. Stripping `kind` there loses it
        // on the wire.
        let Shape::Struct(from_file) = shape_of(&shapes, "FromFile")? else {
            panic!("expected a struct");
        };
        assert!(
            from_file.fields.iter().any(|field| field.wire == "kind"),
            "the response body kept `kind`: {from_file:?}"
        );
        assert_eq!(shape_of(&shapes, "Source")?, &Shape::Any);
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::DiscriminatorEdgeCase)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(
            reported.detail().contains("outside this union"),
            "{reported}"
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
        assert_eq!(union.tag.as_deref(), Some("kind"));
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
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "List": {"type": "array", "items": {"type": "number"}, "minItems": 1},
        })))?;
        assert!(matches!(
            shape_of(&shapes, "Point")?,
            Shape::FixedArray { len: 3, .. }
        ));
        assert!(matches!(shape_of(&shapes, "List")?, Shape::Array { .. }));
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
