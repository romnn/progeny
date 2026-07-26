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
mod merge;
mod roots;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::diag::{Ctx, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;

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
    Tuple {
        items: Vec<ShapeRef>,
        /// What the elements past the tuple may be, when the document allows any.
        rest: Option<ShapeRef>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Union {
    pub(crate) variants: Vec<Variant>,
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
    shapes
}

/// What a shape's own schemas say about themselves, for the doc comments a renderer writes.
pub(crate) fn docs_of(resolved: &ResolvedDocument, key: &ShapeKey) -> Docs {
    classify::docs(resolved, key)
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
        Shape::Tuple { items, rest } => {
            for item in items {
                push(item);
            }
            if let Some(rest) = rest {
                push(rest);
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
    use serde_json::{Value, json};

    use super::{Extra, Scalar, Shape, ShapeRef, Shapes, classify};
    use crate::diag::{Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::{normalize, resolve};

    /// Classify a document, keeping the diagnostics.
    pub(super) fn shapes_of(document: Value) -> (Shapes, Vec<Diagnostic>) {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = classify(&resolved, &mut ctx);
        (shapes, ctx.into_diagnostics())
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
    pub(super) fn shape_of<'a>(shapes: &'a Shapes, name: &str) -> &'a Shape {
        let root = shapes
            .roots()
            .iter()
            .find(|root| root.hint.iter().any(|segment| segment == name))
            .unwrap_or_else(|| panic!("no root named {name}"));
        shapes.get(&root.key).unwrap()
    }

    #[test]
    fn a_reference_is_transparent_so_both_positions_share_one_type() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Alias": {"$ref": "#/components/schemas/Pet"},
        })));
        let pet = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Pet"])
            .unwrap();
        let alias = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Alias"])
            .unwrap();
        // Not two structurally equal types that dedup has to notice afterwards: one key.
        assert_eq!(pet.key, alias.key);
    }

    #[test]
    fn a_reference_with_a_description_beside_it_is_still_a_reference() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Holder": {
                "type": "object",
                "properties": {
                    "pet": {"$ref": "#/components/schemas/Pet", "description": "the pet"},
                },
            },
        })));
        let pet = shapes
            .roots()
            .iter()
            .find(|root| root.hint == ["Pet"])
            .unwrap();
        let Shape::Struct(holder) = shape_of(&shapes, "Holder") else {
            panic!("expected a struct");
        };
        assert_eq!(holder.fields[0].shape, ShapeRef::Key(pet.key.clone()));
        // The description travels with the field rather than making a new type.
        assert_eq!(
            holder.fields[0].docs.description.as_deref(),
            Some("the pet")
        );
    }

    #[test]
    fn a_recursive_schema_is_classified_once_and_terminates() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Node": {
                "type": "object",
                "properties": {"children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}}},
            },
        })));
        let Shape::Struct(node) = shape_of(&shapes, "Node") else {
            panic!("expected a struct");
        };
        let ShapeRef::Key(children) = &node.fields[0].shape else {
            panic!("expected a key");
        };
        let Some(Shape::Array { item: Some(item) }) = shapes.get(children) else {
            panic!("expected an array");
        };
        // The element's key is the struct's own: the cycle is two entries in a map, not a loop.
        assert_eq!(item, &ShapeRef::Key(shape_key_of(&shapes, "Node")));
    }

    fn shape_key_of(shapes: &Shapes, name: &str) -> super::ShapeKey {
        shapes
            .roots()
            .iter()
            .find(|root| root.hint.iter().any(|segment| segment == name))
            .unwrap()
            .key
            .clone()
    }

    #[test]
    fn an_open_object_with_no_properties_is_a_map() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Bag": {"type": "object"},
            "Typed": {"type": "object", "additionalProperties": {"type": "integer"}},
        })));
        assert!(matches!(
            shape_of(&shapes, "Bag"),
            Shape::Map { value: None }
        ));
        assert!(matches!(
            shape_of(&shapes, "Typed"),
            Shape::Map { value: Some(_) }
        ));
    }

    #[test]
    fn additional_properties_false_is_recorded_rather_than_ignored() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Closed": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": false,
            },
        })));
        let Shape::Struct(closed) = shape_of(&shapes, "Closed") else {
            panic!("expected a struct");
        };
        assert_eq!(closed.extra, Extra::Denied);
    }

    #[test]
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
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Struct(child) = shape_of(&shapes, "Child") else {
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

    #[test]
    fn a_property_two_branches_disagree_about_is_merged_one_level_down() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Merged": {
                "allOf": [
                    {"properties": {"a": {"type": "object", "properties": {"x": {"type": "string"}}}}},
                    {"properties": {"a": {"type": "object", "properties": {"y": {"type": "string"}}}}},
                ],
            },
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Struct(merged) = shape_of(&shapes, "Merged") else {
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

    #[test]
    fn an_integer_that_must_also_be_a_number_is_an_integer() {
        // `integer` is a subset of `number`, which the type names hide. `github` writes exactly this
        // and it is not a contradiction.
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Id": {"allOf": [{"type": "number"}, {"type": "integer"}]},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            shape_of(&shapes, "Id"),
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
    }

    #[test]
    fn an_all_of_that_cannot_hold_degrades_and_names_the_conflict() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Impossible": {"allOf": [{"type": "string"}, {"type": "integer"}]},
        })));
        assert_eq!(shape_of(&shapes, "Impossible"), &Shape::Any);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].class(),
            crate::BreakageClass::IrreconcilableAllOf
        );
        assert!(diagnostics[0].detail().contains("no value satisfies both"));
    }

    #[test]
    fn the_nullable_emulation_pattern_is_an_option_and_not_a_union() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "MaybePet": {"anyOf": [{"$ref": "#/components/schemas/Pet"}, {"type": "null"}]},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Optional(ShapeRef::Inline(inner)) = shape_of(&shapes, "MaybePet") else {
            panic!("expected an optional");
        };
        // 83% of every `anyOf` in the corpus is this, and it has an exact translation.
        assert!(matches!(**inner, Shape::Alias(ShapeRef::Key(_))));
    }

    #[test]
    fn a_union_of_constants_is_a_fieldless_enum() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Colour": {"anyOf": [{"const": "red"}, {"const": "green"}]},
            "Direction": {"enum": ["up", "down"]},
        })));
        assert_eq!(
            shape_of(&shapes, "Colour"),
            &Shape::StringEnum(vec!["red".to_owned(), "green".to_owned()])
        );
        assert_eq!(
            shape_of(&shapes, "Direction"),
            &Shape::StringEnum(vec!["up".to_owned(), "down".to_owned()])
        );
    }

    #[test]
    fn a_nullable_enum_is_an_optional_fieldless_enum() {
        // How 3.0's `nullable: true` beside an `enum` arrives after normalization.
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Colour": {"type": ["string", "null"], "enum": ["red", null]},
        })));
        let Shape::Optional(ShapeRef::Inline(inner)) = shape_of(&shapes, "Colour") else {
            panic!("expected an optional");
        };
        assert_eq!(**inner, Shape::StringEnum(vec!["red".to_owned()]));
    }

    #[test]
    fn an_undiscriminated_one_of_is_matched_structurally() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Cat": {"type": "object", "required": ["meow"], "properties": {"meow": {"type": "string"}}},
            "Dog": {"type": "object", "required": ["woof"], "properties": {"woof": {"type": "string"}}},
            "Animal": {"oneOf": [
                {"$ref": "#/components/schemas/Cat"},
                {"$ref": "#/components/schemas/Dog"},
            ]},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Shape::Union(animal) = shape_of(&shapes, "Animal") else {
            panic!("expected a union");
        };
        assert_eq!(animal.variants.len(), 2);
    }

    #[test]
    fn a_discriminated_union_whose_variants_look_alike_degrades_rather_than_guessing() {
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
        })));
        // Matching these structurally would pick whichever deserialized first, which is the one
        // forbidden failure mode. Consuming the tag is stage 4.
        assert_eq!(shape_of(&shapes, "Either"), &Shape::Any);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::DiscriminatorEdgeCase),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_draft_04_tuple_is_a_tuple_once_normalization_has_run() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Pair": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}]},
        })));
        let Shape::Tuple { items, rest } = shape_of(&shapes, "Pair") else {
            panic!("expected a tuple");
        };
        assert_eq!(items.len(), 2);
        assert!(rest.is_none());
    }

    #[test]
    fn a_fixed_length_array_is_told_apart_from_a_list() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "List": {"type": "array", "items": {"type": "number"}, "minItems": 1},
        })));
        assert!(matches!(
            shape_of(&shapes, "Point"),
            Shape::FixedArray { len: 3, .. }
        ));
        assert!(matches!(shape_of(&shapes, "List"), Shape::Array { .. }));
    }

    #[test]
    fn a_validating_keyword_does_not_narrow_the_type_but_is_reported() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Odd": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "not": {"required": ["b"]},
            },
        })));
        // The struct survives: `not` narrows validity, and dropping the type would lose more than
        // ignoring the keyword does.
        assert!(matches!(shape_of(&shapes, "Odd"), Shape::Struct(_)));
        let reported = diagnostics
            .iter()
            .find(|d| d.class() == crate::BreakageClass::UnsupportedConstruct)
            .expect("the keyword should be reported");
        assert!(reported.detail().contains("`not`"), "{reported}");
    }

    #[test]
    fn the_swagger_era_file_type_is_repaired_into_a_binary_string() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Upload": {"type": "file"},
        })));
        assert_eq!(
            shape_of(&shapes, "Upload"),
            &Shape::Format(super::Format::Binary)
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].action(), crate::Action::Repair);
    }

    #[test]
    fn integers_take_their_sign_from_bounds_and_never_their_width() {
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Count": {"type": "integer", "minimum": 0, "maximum": 100},
            "Offset": {"type": "integer", "minimum": -5},
            "Plain": {"type": "integer"},
        })));
        assert_eq!(
            shape_of(&shapes, "Count"),
            &Shape::Scalar(Scalar::Integer { signed: false })
        );
        assert_eq!(
            shape_of(&shapes, "Offset"),
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
        assert_eq!(
            shape_of(&shapes, "Plain"),
            &Shape::Scalar(Scalar::Integer { signed: true })
        );
    }
}
