//! Shapes become provisional contracts: names are assigned and types are spelled out.
//!
//! Which shapes get a name of their own is the decision that governs how much source a document
//! produces. Three rules, in order:
//!
//! 1. A **struct, a fieldless enum or a data-carrying enum** always gets a name — there is no
//!    anonymous form for them in Rust.
//! 2. A **root** — a component, or a schema inline at an API position — gets a name even when its
//!    shape has an anonymous form, because the document named it and checked-in output should say
//!    so. It renders as a type alias.
//! 3. A **recursive** shape gets a name even when neither of the above applies, because a
//!    structural cycle with no name is a type of infinite size. This is the cycle information
//!    resolution computed, used for the decision it was computed for.
//!
//! Everything else is spelled out at the use site. `Vec<String>` is not worth a type, and a
//! document with 642 inline two-element tuples should produce 642 tuples rather than 642 structs.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::name::{self, Namer, RustIdent};
use super::{
    ContractKind, FieldContract, Presence, SkipRule, StringVariant, TaggedVariant, TypeIndex,
    TypeRef, VariantContract,
};
use crate::config::{Config, UnknownFields};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::schema::cycles::Sccs;
use crate::shape::{Docs, Extra, Fit, Format, Scalar, Shape, ShapeKey, ShapeRef, Shapes};

/// One property whose directional distinction was collapsed onto the single shared type.
///
/// Held rather than reported because the diagnostic wants to say which half of the API it affects,
/// and that is a question about positions — see [`crate::api::presence`]. One record shape for
/// every directional fact, so dedup renumbers them all through the same rewrite rather than each
/// growing its own list to forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Collapse {
    pub(crate) owner: TypeIndex,
    pub(crate) at: JsonPointer,
    pub(crate) kind: CollapseKind,
}

/// What the shared type stopped being able to say about the property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollapseKind {
    /// Optional and nullable, folded onto one `Option`.
    Presence,
    /// `readOnly: true` — the document says only responses carry the member.
    ReadOnly,
    /// `writeOnly: true` — the document says only requests carry the member.
    WriteOnly,
}

/// A contract before the caller's policy and the eligibility rules have been applied.
#[derive(Debug, Clone)]
pub(super) struct Provisional {
    pub(super) rust_name: RustIdent,
    pub(super) docs: Docs,
    pub(super) kind: ContractKind,
    pub(super) unknown_fields: UnknownFields,
    pub(super) origin: JsonPointer,
    /// The document gave this shape a name of its own, so it must not merge with another type that
    /// also has one: two named components evolve independently, whatever they look like today.
    pub(super) declared: bool,
    /// The caller named it, which is a declaration of identity and blocks merging outright.
    pub(super) explicit: bool,
    /// The `components.schemas` name, when the document gave one — the key a caller uses to talk
    /// about this type in the configuration.
    pub(super) component: Option<String>,
}

/// Lower every shape, and say which type each one became.
///
/// The second half of the return value is what the API model reads: an operation names a schema,
/// and the only sound answer to "which Rust type is that" is the one the type layer already gave.
pub(super) fn run(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    config: &Config,
    ctx: &mut Ctx,
) -> Lowered {
    let mut lower = Lower {
        resolved,
        shapes,
        config,
        namer: Namer::default(),
        done: BTreeMap::new(),
        types: Vec::new(),
        hints: BTreeMap::new(),
        by_address: BTreeMap::new(),
        components: BTreeMap::new(),
        needs_name: BTreeSet::new(),
        collapses: Vec::new(),
    };

    for root in shapes.roots() {
        lower
            .hints
            .entry(root.key.clone())
            .or_insert_with(|| root.hint.clone());
        if let Some(address) = shapes.address(&root.key) {
            lower
                .by_address
                .entry(address.to_string())
                .or_insert_with(|| root.hint.clone());
            if root.kind == crate::shape::RootKind::Component
                && let Some(name) = root.hint.first()
            {
                lower
                    .components
                    .entry(address.to_string())
                    .or_insert_with(|| name.clone());
            }
        }
    }

    // Only a component forces a name for a shape that has an anonymous form. An inline position
    // contributes its *name* — so a struct inside a request body is called after the operation — but
    // a query parameter whose schema is a plain integer does not need `pub type ListPetsLimit = i64;`
    // in the output, and emitting one is noise in every generated crate.
    let declared: BTreeSet<ShapeKey> = shapes
        .roots()
        .iter()
        .filter(|root| root.kind == crate::shape::RootKind::Component)
        .map(|root| root.key.clone())
        .collect();
    lower.needs_name = names_needed(shapes, &declared);
    for root in shapes.roots() {
        lower.key(&root.key, ctx);
    }
    Lowered {
        types: lower.types,
        by_shape: lower.done,
        collapses: lower.collapses,
    }
}

/// Everything lowering produced.
pub(super) struct Lowered {
    pub(super) types: Vec<Provisional>,
    pub(super) by_shape: BTreeMap<ShapeKey, TypeRef>,
    pub(super) collapses: Vec<Collapse>,
}

/// Which keys have to become named types.
///
/// The shapes with no anonymous form and the roots are obvious. The third group is not: a chain of
/// *anonymous* shapes that comes back to itself has no Rust form at all, because spelling it out
/// would never terminate — `Vec<Vec<Vec<…>>>`. So the anonymous shapes are checked for cycles among
/// themselves, and any that lie on one is given a name.
///
/// Restricting the graph to the anonymous keys is what keeps this from over-naming. Every schema in
/// a recursive group is *reachable* from a cycle, but only the ones on a cycle that no named type
/// interrupts actually need a name of their own — otherwise a list inside a recursive struct would
/// acquire a type nobody asked for.
fn names_needed(shapes: &Shapes, declared: &BTreeSet<ShapeKey>) -> BTreeSet<ShapeKey> {
    let mut needed: BTreeSet<ShapeKey> = shapes
        .keys()
        .filter(|key| declared.contains(*key) || shapes.get(key).is_some_and(nameable))
        .cloned()
        .collect();

    let anonymous: Vec<&ShapeKey> = shapes.keys().filter(|key| !needed.contains(*key)).collect();
    let numbered: BTreeMap<&ShapeKey, u32> = anonymous
        .iter()
        .enumerate()
        .filter_map(|(index, key)| u32::try_from(index).ok().map(|index| (*key, index)))
        .collect();

    let mut edges = Vec::new();
    for (key, &from) in &numbered {
        let Some(shape) = shapes.get(key) else {
            continue;
        };
        for child in crate::shape::child_keys(shape) {
            if let Some(&to) = numbered.get(&child) {
                edges.push((from, to));
            }
        }
    }
    let cycles = Sccs::of(anonymous.len(), &edges);
    for (key, &index) in &numbered {
        if cycles.recursive(index as usize) {
            needed.insert((*key).clone());
        }
    }
    needed
}

struct Lower<'a> {
    resolved: &'a ResolvedDocument,
    shapes: &'a Shapes,
    config: &'a Config,
    namer: Namer,
    /// The type each key lowers to, so a shape reached twice is one type.
    done: BTreeMap<ShapeKey, TypeRef>,
    types: Vec<Provisional>,
    /// The name a root asked for.
    hints: BTreeMap<ShapeKey, Vec<String>>,
    /// The same, by the address of the root, for naming the shapes inside it.
    by_address: BTreeMap<String, Vec<String>>,
    /// Which addresses are `components.schemas` entries, and under what name.
    components: BTreeMap<String, String>,
    /// Every key that has to become a named type.
    needs_name: BTreeSet<ShapeKey>,
    /// Properties whose optional-and-nullable distinction was collapsed, awaiting a position.
    collapses: Vec<Collapse>,
}

/// A wrapper peeled off a shape before its core is named.
#[derive(Clone, Copy)]
enum Wrap {
    Option,
}

impl Lower<'_> {
    /// The type a key lowers to, creating one if this is the first time it is asked for.
    fn key(&mut self, key: &ShapeKey, ctx: &mut Ctx) -> TypeRef {
        if let Some(existing) = self.done.get(key) {
            return existing.clone();
        }
        let Some(shape) = self.shapes.get(key) else {
            // A key with no shape cannot occur — classification reaches every key the work list
            // held — and if it did, "anything" is the sound answer.
            return TypeRef::Value;
        };
        let (wraps, core) = peel(shape);

        if self.needs_name.contains(key) {
            let index = self.reserve(key, &wraps, ctx);
            let core = core.clone();
            let kind = self.kind(&core, key, index, ctx);
            if let Some(slot) = self.types.get_mut(index.index()) {
                slot.kind = kind;
            }
            return self.done.get(key).cloned().unwrap_or(TypeRef::Value);
        }

        let lowered = wrap(&wraps, self.inline(core, key, ctx));
        self.done.insert(key.clone(), lowered.clone());
        lowered
    }

    /// Claim a name and an index for this key before its children are lowered.
    ///
    /// The order matters: a recursive shape reaches itself while its own children are being
    /// lowered, and it has to find the type it is going to be rather than start again.
    fn reserve(&mut self, key: &ShapeKey, wraps: &[Wrap], ctx: &mut Ctx) -> TypeIndex {
        let address = self
            .shapes
            .address(key)
            .cloned()
            .unwrap_or_else(JsonPointer::root);
        let rendered = address.to_string();
        let component = self.components.get(&rendered).cloned();
        let segments = self.segments(key, &rendered);

        let (ident, explicit) = if let Some(chosen) =
            self.config.name_for(component.as_deref(), &rendered)
        {
            (RustIdent::type_name(std::slice::from_ref(chosen)), true)
        } else {
            {
                let named = self.namer.claim(&segments);
                if named.collided_with.is_some() {
                    // The sentence names no identifier on purpose: a detail that varies per
                    // occurrence cannot be aggregated, and `zoom` alone collides 19 times. The count
                    // is the finding, and the locations say where to look.
                    ctx.report(Diagnostic::new(
                        BreakageClass::CollidingTypeName,
                        Action::Warn,
                        address.clone(),
                        "the name this shape's position asks for is taken already, so it was given a \
                         numeric suffix; a `names` entry in the configuration can give it a better \
                         one",
                    ));
                }
                (named.ident, false)
            }
        };

        let index = TypeIndex(u32::try_from(self.types.len()).unwrap_or(u32::MAX));
        self.types.push(Provisional {
            rust_name: ident,
            docs: crate::shape::docs_of(self.resolved, key),
            // Replaced as soon as the children are lowered; a struct with no fields is the least
            // surprising thing to see if that ever failed to happen.
            kind: ContractKind::Struct { fields: Vec::new() },
            unknown_fields: self
                .config
                .unknown_fields_for(component.as_deref(), &rendered),
            origin: address,
            declared: component.is_some(),
            explicit,
            component,
        });
        self.done
            .insert(key.clone(), wrap(wraps, TypeRef::Named(index)));
        index
    }

    /// The name segments for a shape: the root's own hint, or the nearest root's plus the way down.
    fn segments(&self, key: &ShapeKey, address: &str) -> Vec<String> {
        if let Some(hint) = self.hints.get(key) {
            return hint.clone();
        }
        let Some(pointer) = JsonPointer::parse(address) else {
            return vec!["Type".to_owned()];
        };
        let tokens = pointer.tokens();
        // The longest root address that this one sits inside names the ancestor; the tokens past it
        // say where inside. Walking down from the full address means the *nearest* root wins.
        for split in (0..=tokens.len()).rev() {
            let prefix = tokens.get(..split).unwrap_or_default();
            let rendered = prefix
                .iter()
                .fold(JsonPointer::root(), |at, token| at.child(token.clone()))
                .to_string();
            let Some(hint) = self.by_address.get(&rendered) else {
                continue;
            };
            let mut segments = hint.clone();
            let rest = tokens.get(split..).unwrap_or_default();
            for (offset, token) in rest.iter().enumerate() {
                let previous = if offset == 0 {
                    prefix.last()
                } else {
                    rest.get(offset - 1)
                };
                if let Some(segment) = name::segment(token, previous.map(String::as_str)) {
                    segments.push(segment);
                }
            }
            return segments;
        }
        // No root above it at all: name it after its whole address, which is still deterministic.
        tokens.to_vec()
    }

    /// The kind of item a named type is.
    fn kind(
        &mut self,
        core: &Shape,
        key: &ShapeKey,
        index: TypeIndex,
        ctx: &mut Ctx,
    ) -> ContractKind {
        match core {
            Shape::Struct(structure) => {
                let (fields, unknown) = self.fields(structure, key, index, ctx);
                if let Some(slot) = self.types.get_mut(index.index()) {
                    slot.unknown_fields = unknown;
                }
                ContractKind::Struct { fields }
            }
            Shape::StringEnum(values) => ContractKind::StringEnum {
                variants: string_variants(values),
            },
            Shape::Union(union) => {
                let variants = union
                    .variants
                    .iter()
                    .map(|variant| {
                        (
                            self.reference(&variant.shape, key, ctx),
                            variant.tag.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                match &union.tag {
                    None => ContractKind::Enum {
                        variants: self.variants(variants.into_iter().map(|(ty, _)| ty).collect()),
                    },
                    Some(tag) => {
                        if let Some(variants) = tagged_variants(variants) {
                            ContractKind::TaggedEnum {
                                tag: tag.clone(),
                                variants,
                            }
                        } else {
                            // A tagged union with a variant nothing names is what
                            // [`crate::shape::discriminate`] demotes before lowering ever runs,
                            // so this arm is the pipeline contract breaking rather than a
                            // document shape. The same demotion is applied here, because the
                            // alternatives are both the forbidden failure mode: rendering the
                            // union untagged matches whichever branch parses first, and rendering
                            // it tagged writes a Rust variant name progeny made up onto the wire.
                            ctx.report(Diagnostic::new(
                                BreakageClass::DiscriminatorEdgeCase,
                                Action::Degrade,
                                self.types
                                    .get(index.index())
                                    .map_or_else(JsonPointer::root, |slot| slot.origin.clone()),
                                "this union declares a discriminator, but nothing says what one \
                                 of its variants' tags would read; the value is typed as \
                                 arbitrary JSON rather than tagged with an invented name",
                            ));
                            ContractKind::Alias {
                                target: TypeRef::Value,
                            }
                        }
                    }
                }
            }
            Shape::Tuple { items, .. } => ContractKind::Tuple {
                items: items
                    .iter()
                    .map(|item| self.reference(item, key, ctx))
                    .collect(),
            },
            // A root whose shape has an anonymous form is a name for that form. Spelled variant
            // by variant rather than wildcarded: `nameable` and `inline` each hold their own
            // copy of "which shapes have no anonymous form", and a new shape falling into a
            // wildcard here would lower to a silent `pub type X = …` alias whether or not that
            // was the decision — the compiler hands the new variant's author all three sites
            // instead.
            other @ (Shape::Null
            | Shape::Scalar(_)
            | Shape::Format(_)
            | Shape::Map { .. }
            | Shape::Array { .. }
            | Shape::FixedArray { .. }
            | Shape::Optional(_)
            | Shape::Alias(_)
            | Shape::Any) => ContractKind::Alias {
                target: self.inline(other, key, ctx),
            },
        }
    }

    /// The type a shape with an anonymous form spells out.
    fn inline(&mut self, shape: &Shape, key: &ShapeKey, ctx: &mut Ctx) -> TypeRef {
        match shape {
            Shape::Null => TypeRef::Unit,
            Shape::Scalar(Scalar::Bool) => TypeRef::Bool,
            Shape::Scalar(Scalar::Integer { signed: true }) => TypeRef::I64,
            Shape::Scalar(Scalar::Integer { signed: false }) => TypeRef::U64,
            Shape::Scalar(Scalar::Number) => TypeRef::F64,
            Shape::Scalar(Scalar::String) => TypeRef::String,
            Shape::Format(format) => TypeRef::Format(*format),
            Shape::Array { item } => TypeRef::Vec(Box::new(self.element(item.as_ref(), key, ctx))),
            Shape::Map { value } => TypeRef::Map(Box::new(self.element(value.as_ref(), key, ctx))),
            Shape::FixedArray { item, len } => {
                TypeRef::Array(Box::new(self.reference(item, key, ctx)), *len)
            }
            Shape::Tuple { items, .. } => TypeRef::Tuple(
                items
                    .iter()
                    .map(|item| self.reference(item, key, ctx))
                    .collect(),
            ),
            Shape::Optional(inner) | Shape::Alias(inner) => self.reference(inner, key, ctx),
            // `Any` is arbitrary JSON by definition. The three nameable shapes are reached only when
            // one is asked for inline, which `needs_name` prevents; arbitrary JSON is the sound
            // answer there too.
            Shape::Any | Shape::Struct(_) | Shape::StringEnum(_) | Shape::Union(_) => {
                TypeRef::Value
            }
        }
    }

    fn element(&mut self, value: Option<&ShapeRef>, key: &ShapeKey, ctx: &mut Ctx) -> TypeRef {
        match value {
            Some(value) => self.reference(value, key, ctx),
            // A list or map the document did not give an element type: anything may be in it.
            None => TypeRef::Value,
        }
    }

    /// The type a child position lowers to.
    fn reference(&mut self, reference: &ShapeRef, parent: &ShapeKey, ctx: &mut Ctx) -> TypeRef {
        match reference {
            ShapeRef::Key(key) => self.key(key, ctx),
            ShapeRef::Inline(shape) => {
                let (wraps, core) = peel(shape);
                wrap(&wraps, self.inline(core, parent, ctx))
            }
        }
    }

    /// A struct's fields, and the unknown-member policy its schema implies.
    fn fields(
        &mut self,
        structure: &crate::shape::Struct,
        key: &ShapeKey,
        index: TypeIndex,
        ctx: &mut Ctx,
    ) -> (Vec<FieldContract>, UnknownFields) {
        let mut used = Namer::default();
        let mut fields = Vec::new();
        let origin = self
            .types
            .get(index.index())
            .map_or_else(JsonPointer::root, |contract| contract.origin.clone());

        for field in &structure.fields {
            let nullable = matches!(self.shape_of(&field.shape), Some(Shape::Optional(_)));
            let inner = self.reference(&field.shape, key, ctx);
            let position = origin.child("properties").child(field.wire.clone());
            // An access marker names the direction the member travels; whether this type also
            // crosses the *other* direction is the API model's knowledge, so the fact is recorded
            // here and priced there — the same split the presence collapse below uses. Dropping
            // the markers with no record made a `readOnly` member a silent fidelity gap.
            if field.read_only {
                self.collapses.push(Collapse {
                    owner: index,
                    at: position.clone(),
                    kind: CollapseKind::ReadOnly,
                });
            }
            if field.write_only {
                self.collapses.push(Collapse {
                    owner: index,
                    at: position.clone(),
                    kind: CollapseKind::WriteOnly,
                });
            }
            let (presence, ty, skip) = match (field.required, nullable) {
                (true, false) => (Presence::Required, inner, SkipRule::Never),
                // Already an `Option` from the shape: the key is there and may be null.
                (true, true) => (Presence::Nullable, inner, SkipRule::Never),
                (false, false) => (
                    Presence::Optional,
                    TypeRef::Option(Box::new(inner)),
                    SkipRule::WhenNone,
                ),
                (false, true) => {
                    // Recorded rather than reported. The finding is real here, but *which half of
                    // the API it costs* is not knowable here: a collapse on the way in loses a
                    // caller's explicit null, and one on the way out loses a server's — different
                    // consequences, and nothing knows a position until the API model exists.
                    self.collapses.push(Collapse {
                        owner: index,
                        at: position,
                        kind: CollapseKind::Presence,
                    });
                    (Presence::OptionalNullable, inner, SkipRule::WhenNone)
                }
            };
            let default = self.default(field, &origin, ctx);
            fields.push(FieldContract {
                rust_name: unique_field(&mut used, &field.wire),
                wire_name: field.wire.clone(),
                ty,
                presence,
                default,
                skip_serializing_if: skip,
                flatten: false,
                docs: field.docs.clone(),
            });
        }

        let policy = self
            .types
            .get(index.index())
            .map_or(UnknownFields::Ignore, |contract| contract.unknown_fields);
        let (captured, policy) = match &structure.extra {
            // The document says what an undeclared member may be, which is a statement the
            // caller's policy does not get to override: capturing them is the faithful reading.
            Extra::Typed(value) => {
                let element = self.reference(value, key, ctx);
                (Some(element), UnknownFields::Capture)
            }
            Extra::Denied => (None, UnknownFields::Deny),
            Extra::Open => match policy {
                UnknownFields::Capture => (Some(TypeRef::Value), UnknownFields::Capture),
                other => (None, other),
            },
        };
        if let Some(element) = captured {
            fields.push(FieldContract {
                rust_name: unique_field(&mut used, "extra"),
                // A flattened member has no wire name of its own: it *is* the leftovers.
                wire_name: String::new(),
                ty: TypeRef::Map(Box::new(element)),
                presence: Presence::Required,
                default: None,
                skip_serializing_if: SkipRule::Never,
                flatten: true,
                docs: Docs {
                    title: None,
                    description: Some(
                        "Members the description did not declare, kept verbatim.".to_owned(),
                    ),
                    deprecated: false,
                },
            });
        }
        (fields, policy)
    }

    fn shape_of<'a>(&'a self, reference: &'a ShapeRef) -> Option<&'a Shape> {
        match reference {
            ShapeRef::Key(key) => self.shapes.get(key),
            ShapeRef::Inline(shape) => Some(shape),
        }
    }

    /// Keep a default only if it could be a value of the field's own schema.
    ///
    /// The rule is [`crate::shape::Fit`] — the same one the example checker runs, so the two
    /// verdicts about one JSON value cannot disagree. It is one-sided on purpose: a default is
    /// dropped only when it certainly cannot deserialize into the generated type, because a
    /// default that does not typecheck is worse than none, and one dropped on a guess is worse
    /// than either.
    fn default(
        &self,
        field: &crate::shape::Field,
        origin: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<Value> {
        let default = field.default.as_ref()?;
        // A `null` default on an optional member says the server assumes nothing when the member
        // is absent. The generated field is an `Option` precisely because the member may be
        // absent, so `null` deserializes into it whatever the property's own schema says.
        if default.is_null() && !field.required {
            return Some(default.clone());
        }
        let Some(reason) = Fit::new(self.shapes, "the default").through(default, &field.shape)
        else {
            return Some(default.clone());
        };
        ctx.report(Diagnostic::new(
            BreakageClass::InvalidDefault,
            Action::Repair,
            origin
                .child("properties")
                .child(field.wire.clone())
                .child("default"),
            format!(
                "the declared default contradicts the property's own schema: {reason}; dropped \
                 it, because a default that does not typecheck is worse than none"
            ),
        ));
        None
    }

    /// Give every variant of an untagged union a distinct name.
    ///
    /// An untagged union has no wire name to use — nothing on the wire says which variant a
    /// payload is — so the name falls back to the type each variant holds.
    fn variants(&self, types: Vec<TypeRef>) -> Vec<VariantContract> {
        let mut used = Namer::default();
        types
            .into_iter()
            .map(|ty| VariantContract {
                rust_name: used.unique(RustIdent::variant(&self.variant_name(&ty))),
                ty,
            })
            .collect()
    }

    /// What to call a variant holding this type.
    fn variant_name(&self, ty: &TypeRef) -> String {
        match ty {
            TypeRef::Named(index) => self
                .types
                .get(index.index())
                .map_or_else(|| "Variant".to_owned(), |it| it.rust_name.to_string()),
            TypeRef::Unit => "Null".to_owned(),
            TypeRef::Bool => "Boolean".to_owned(),
            TypeRef::I64 | TypeRef::U64 => "Integer".to_owned(),
            TypeRef::F64 => "Number".to_owned(),
            TypeRef::String => "Text".to_owned(),
            TypeRef::Format(format) => format_name(*format).to_owned(),
            TypeRef::Value => "Any".to_owned(),
            TypeRef::Option(inner) | TypeRef::Boxed(inner) => self.variant_name(inner),
            TypeRef::Vec(inner) | TypeRef::Array(inner, _) => {
                format!("{}List", self.variant_name(inner))
            }
            TypeRef::Map(inner) => format!("{}Map", self.variant_name(inner)),
            TypeRef::Tuple(_) => "Tuple".to_owned(),
        }
    }
}

/// Pair every variant of a tagged union with the tag value that names it, or nothing if any value
/// is missing.
///
/// The names are the tag values themselves, because that is what the document calls the variants
/// and what a reader of a payload sees. The `None` is the discharge of the shape layer's
/// proposal-phase `Option`: a value may legitimately be unknown while a tag is merely *proposed*,
/// but [`crate::shape::discriminate`] demotes every union where one stayed unknown, so a settled
/// tagged union always has all of them — and the caller treats `None` as that guarantee having
/// broken.
fn tagged_variants(types: Vec<(TypeRef, Option<String>)>) -> Option<Vec<TaggedVariant>> {
    let mut used = Namer::default();
    types
        .into_iter()
        .map(|(ty, tag)| {
            let tag_value = tag?;
            Some(TaggedVariant {
                rust_name: used.unique(RustIdent::variant(&tag_value)),
                ty,
                tag_value,
            })
        })
        .collect()
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::DateTime => "DateTime",
        Format::Date => "Date",
        Format::Time => "Time",
        Format::Uuid => "Uuid",
        Format::Base64 => "Base64",
        Format::Binary => "Binary",
    }
}

/// Peel the wrappers a shape puts around the thing that needs a name.
///
/// `type: ["object", "null"]` with properties is an `Option` of a struct, and it is the *struct*
/// that needs the name — so the wrapper comes off before naming and goes back on afterwards.
fn peel(shape: &Shape) -> (Vec<Wrap>, &Shape) {
    let mut wraps = Vec::new();
    let mut current = shape;
    loop {
        match current {
            Shape::Optional(ShapeRef::Inline(inner)) => {
                wraps.push(Wrap::Option);
                current = inner;
            }
            Shape::Alias(ShapeRef::Inline(inner)) => current = inner,
            _ => return (wraps, current),
        }
    }
}

fn wrap(wraps: &[Wrap], mut ty: TypeRef) -> TypeRef {
    for _ in wraps.iter().rev() {
        ty = TypeRef::Option(Box::new(ty));
    }
    ty
}

/// Whether a shape has no anonymous Rust form.
fn nameable(shape: &Shape) -> bool {
    matches!(
        shape,
        Shape::Struct(_) | Shape::StringEnum(_) | Shape::Union(_)
    )
}

fn string_variants(values: &[String]) -> Vec<StringVariant> {
    let mut used = Namer::default();
    values
        .iter()
        .map(|value| StringVariant {
            rust_name: used.unique(RustIdent::variant(value)),
            wire_name: value.clone(),
        })
        .collect()
}

/// A field name that no other field of the same struct already has.
fn unique_field(used: &mut Namer, wire: &str) -> RustIdent {
    used.unique(RustIdent::field(wire))
}
