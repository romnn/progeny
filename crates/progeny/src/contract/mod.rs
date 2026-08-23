//! The wire contract: one record per generated type, and the only thing a renderer reads.
//!
//! This is the whole of Lesson 2. Every choice that affects the bytes on the wire — the name a
//! field serializes under, whether an absent key is legal, how a union is tagged, how long a tuple
//! is, which derives appear — is decided here and stored as data. A renderer receives
//! `&TypeContract` and has nowhere to put a decision, so the two historical bugs (a customization
//! changing the wire contract after eligibility had been decided; a fixed-arity check dropped in
//! the buffering path) have no phase to live in.
//!
//! The order is load-bearing: shapes are lowered into *provisional* contracts, the caller's
//! customization is applied to those, and only then does [`finalize`] decide derive eligibility and
//! the serde strategy. Nothing may run after `finalize`.
//!
//! *Enforcement:* [`TypeContract`]'s fields are private and it has no public constructor; the only
//! thing that builds one is [`finalize::run`], which consumes the provisional form and the
//! [`Config`] together.

mod dedup;
mod finalize;
mod lower;
mod name;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::{Config, Derive, UnknownFields};
use crate::diag::{Ctx, JsonPointer};
use crate::resolve::ResolvedDocument;
pub(crate) use crate::shape::Format;
use crate::shape::{Docs, ShapeKey, Shapes};

#[cfg(feature = "harness")]
pub(crate) use finalize::BASE as BASE_DERIVES;
pub(crate) use lower::{Collapse, CollapseKind};
pub(crate) use name::{Namer, RustIdent};

/// Which generated type, by position in [`Contracts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TypeIndex(u32);

impl TypeIndex {
    fn index(self) -> usize {
        self.0 as usize
    }

    /// The index as a graph node, for the cycle analysis.
    fn raw(self) -> u32 {
        self.0
    }
}

/// A Rust type, as a renderer needs to write it.
///
/// Structural rather than named wherever it can be: only [`TypeRef::Named`] needs identity, and
/// everything else — an option, a list, a map — is spelled out at the use site, so there is no
/// generated type whose entire content is `Vec<String>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TypeRef {
    Named(TypeIndex),
    /// `()`. What a `type: "null"` schema and an empty response body are.
    Unit,
    Bool,
    /// `i64`. Never narrower: see [`crate::shape::Scalar::Integer`].
    I64,
    U64,
    F64,
    String,
    /// A format with a `Config`-chosen type.
    Format(Format),
    /// `support::Upload`: a binary member of a `multipart/form-data` body.
    ///
    /// Only there — a binary string anywhere else stays [`TypeRef::Format`]. A file part is
    /// bytes, which a `String` cannot hold, and it carries a `filename` and its own
    /// `Content-Type`, which no schema property declares; the support type is the one place all
    /// three fit.
    Upload,
    /// `serde_json::Value`: the degradation target.
    Value,
    Option(Box<TypeRef>),
    Vec(Box<TypeRef>),
    /// The `Config`-chosen map type, keyed by `String`.
    Map(Box<TypeRef>),
    /// `[T; N]`.
    Array(Box<TypeRef>, u32),
    Tuple(Vec<TypeRef>),
    /// An indirection, placed to break a cycle rustc would otherwise reject.
    Boxed(Box<TypeRef>),
}

impl TypeRef {
    /// Whether the spelled-out type names `support::Upload` anywhere.
    ///
    /// Exhaustive so a future container variant cannot silently hide an upload from the
    /// support-module decision that reads this.
    pub(crate) fn holds_upload(&self) -> bool {
        match self {
            Self::Upload => true,
            Self::Option(inner)
            | Self::Vec(inner)
            | Self::Map(inner)
            | Self::Array(inner, _)
            | Self::Boxed(inner) => inner.holds_upload(),
            Self::Tuple(items) => items.iter().any(Self::holds_upload),
            // A named type is its own contract, scanned on its own turn.
            Self::Named(_)
            | Self::Unit
            | Self::Bool
            | Self::I64
            | Self::U64
            | Self::F64
            | Self::String
            | Self::Format(_)
            | Self::Value => false,
        }
    }

    /// The named types this reference reaches, and whether it reaches them indirectly.
    ///
    /// A cycle through a `Vec`, a map or a `Box` is fine; a cycle through none of them is a type of
    /// infinite size. `Option<T>` indirects nothing — it is `T`'s size plus a discriminant — which
    /// is why it passes the flag through unchanged.
    fn reaches(&self, indirect: bool, out: &mut Vec<(TypeIndex, bool)>) {
        match self {
            Self::Named(index) => out.push((*index, indirect)),
            // Neither an option nor a fixed array indirects: both are their content's size plus at
            // most a discriminant.
            Self::Option(inner) | Self::Array(inner, _) => inner.reaches(indirect, out),
            Self::Vec(inner) | Self::Map(inner) | Self::Boxed(inner) => inner.reaches(true, out),
            Self::Tuple(items) => {
                for item in items {
                    item.reaches(indirect, out);
                }
            }
            Self::Unit
            | Self::Bool
            | Self::I64
            | Self::U64
            | Self::F64
            | Self::String
            | Self::Format(_)
            | Self::Upload
            | Self::Value => {}
        }
    }

    /// Every named type this reference reaches, however it is wrapped.
    pub(crate) fn named(&self, out: &mut Vec<TypeIndex>) {
        let mut reached = Vec::new();
        self.reaches(false, &mut reached);
        out.extend(reached.into_iter().map(|(index, _)| index));
    }

    /// Rewrite every named reference through `remap`, which dedup fills in.
    ///
    /// Exhaustive like [`Self::reaches`] above, and for a sharper reason: a composite variant a
    /// wildcard arm absorbed here would keep its *stale* indices after dedup renumbers — a valid,
    /// wrong type name in the output, which is the forbidden failure mode. The compiler hands the
    /// next variant's author this match; a `_` would hand them nothing.
    fn remap(&mut self, remap: &BTreeMap<TypeIndex, TypeIndex>) {
        match self {
            Self::Named(index) => {
                if let Some(&target) = remap.get(index) {
                    *index = target;
                }
            }
            Self::Option(inner)
            | Self::Vec(inner)
            | Self::Map(inner)
            | Self::Boxed(inner)
            | Self::Array(inner, _) => inner.remap(remap),
            Self::Tuple(items) => {
                for item in items {
                    item.remap(remap);
                }
            }
            Self::Unit
            | Self::Bool
            | Self::I64
            | Self::U64
            | Self::F64
            | Self::String
            | Self::Format(_)
            | Self::Upload
            | Self::Value => {}
        }
    }
}

/// Whether a member may be absent, null, both, or neither.
///
/// Kept even though v1 collapses the last two onto one `Option`, because it is the fact the
/// document stated and the thing a later `Patch<T>` would need. The collapse is diagnosed rather
/// than silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Presence {
    /// The key is always there and never null.
    Required,
    /// The key may be absent.
    Optional,
    /// The key is always there and may be null.
    Nullable,
    /// The key may be absent *or* null, and the document says those are different.
    OptionalNullable,
}

/// When a member is left out of the serialized form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SkipRule {
    /// Always written, `null` included.
    Never,
    /// Left out when it is `None`.
    WhenNone,
}

/// Which `Deserialize` implementation a type gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum DeserStrategy {
    /// The serde derive. The escape hatch, and the only strategy in `DeriveAlways` mode.
    Derive,
    /// Hand-written, buffering the members before assigning them: the compile-speed path.
    HandWrittenBuffered {
        /// Whether the emitted implementation refuses an undeclared member.
        ///
        /// Resolved here by the eligibility ruling, which sends `Capture` to the derive — so by
        /// the time this strategy exists, capturing is impossible and the two remaining policies
        /// are one bit. Carried in the strategy so the renderer receives the answer instead of
        /// re-deriving it: a renderer-side `Capture → Ignore` arm once encoded "cannot arrive
        /// here" as a silent fold, which would have discarded members without a diagnostic the
        /// day the eligibility rule loosened.
        deny_unknown: bool,
    },
    /// Hand-written with no buffering, for a fieldless enum.
    HandWrittenFieldless,
    /// Hand-written, buffering the payload to read its tag member, for a carried-tag enum.
    ///
    /// The only strategy such an enum can take, whatever the configuration says: serde's derive
    /// repertoire — external, `tag`, `tag`+`content`, `untagged` — has no encoding that reads a
    /// tag and leaves it in the payload. Nothing is conceded to non-self-describing formats by
    /// this, because the encodings a tagged union could otherwise take (`tag = "…"`, `untagged`)
    /// buffer through serde's private `Content` and already require a self-describing format.
    HandWrittenCarriedTag,
}

/// What kind of Rust item a type is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContractKind {
    Struct {
        fields: Vec<FieldContract>,
    },
    /// A data-carrying enum matched by shape, and the common case: the variants differ in a way a
    /// payload shows, so nothing on the wire names them.
    Enum {
        variants: Vec<VariantContract>,
    },
    /// A data-carrying enum whose payload names its own variant in a member, which the union
    /// consumes.
    ///
    /// Used only where matching by shape would be unsound, because consuming the tag costs each
    /// variant type the property that carries it ([`crate::shape`]). A kind of its own rather than
    /// a tagging flag beside [`ContractKind::Enum`], because the two say different things about a
    /// variant: a tagged variant always has the exact bytes its tag member reads, and an untagged
    /// one never does. As two kinds with two variant types, a tagged variant without a value —
    /// which serde would fill by writing the *Rust* variant name onto the wire — cannot be
    /// constructed at all, where it used to be a pairing four comments asserted and nothing
    /// enforced.
    TaggedEnum {
        /// The member of every payload that carries its variant's name.
        tag: String,
        variants: Vec<TaggedVariant>,
    },
    /// A data-carrying enum whose payload names its own variant in a member that the variant
    /// types keep: the union reads the member to pick a variant and replays the whole payload —
    /// tag included — into it, and serializing writes the variant as it is, which already carries
    /// its tag.
    ///
    /// Used where matching by shape would be unsound *and* the tag cannot be consumed, because
    /// some variant type also puts the property on the wire somewhere else ([`crate::shape`]). A
    /// kind of its own rather than a flag beside [`ContractKind::TaggedEnum`] for the same reason
    /// that kind is one beside [`ContractKind::Enum`]: the two render nothing alike — a consumed
    /// tag is a serde attribute, a carried one is a pair of hand-written impls — and a flag would
    /// put that ruling back inside the renderer.
    CarriedTagEnum {
        /// The member of every payload that carries its variant's name.
        tag: String,
        variants: Vec<TaggedVariant>,
    },
    /// An enum with no data: the fast serde path.
    StringEnum {
        variants: Vec<StringVariant>,
    },
    /// A wrapper with an identity of its own.
    Newtype {
        inner: TypeRef,
    },
    /// A fixed-arity sequence. The arity is `items.len()`, which is why the end-of-sequence check
    /// the serde renderer emits comes from data rather than from a code path someone has to
    /// remember.
    Tuple {
        items: Vec<TypeRef>,
    },
    /// A name for another type.
    Alias {
        target: TypeRef,
    },
}

impl ContractKind {
    /// Every type reference this kind holds.
    pub(crate) fn references(&self) -> Vec<&TypeRef> {
        match self {
            Self::Struct { fields } => fields.iter().map(|field| &field.ty).collect(),
            Self::Enum { variants } => variants.iter().map(|variant| &variant.ty).collect(),
            Self::TaggedEnum { variants, .. } | Self::CarriedTagEnum { variants, .. } => {
                variants.iter().map(|variant| &variant.ty).collect()
            }
            Self::StringEnum { .. } => Vec::new(),
            Self::Newtype { inner } | Self::Alias { target: inner } => vec![inner],
            Self::Tuple { items } => items.iter().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FieldContract {
    pub(crate) rust_name: RustIdent,
    /// The exact bytes the member has on the wire.
    pub(crate) wire_name: String,
    pub(crate) ty: TypeRef,
    pub(crate) presence: Presence,
    /// Validated against `ty`, or dropped with a diagnostic. A default that does not typecheck is
    /// worse than no default.
    pub(crate) default: Option<Value>,
    /// Derived from `presence`, never free-form.
    pub(crate) skip_serializing_if: SkipRule,
    /// Set only for the member that captures unknown fields.
    pub(crate) flatten: bool,
    pub(crate) docs: Docs,
}

/// One variant of an untagged union: a name for the reader, a type for the payload, and nothing
/// for the wire, because an untagged union writes no variant names there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariantContract {
    pub(crate) rust_name: RustIdent,
    pub(crate) ty: TypeRef,
}

/// One variant of a tagged union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaggedVariant {
    pub(crate) rust_name: RustIdent,
    pub(crate) ty: TypeRef,
    /// The exact bytes the tag member holds for this variant — what lets the renderer write a
    /// `rename` without asking whether it should.
    pub(crate) tag_value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StringVariant {
    pub(crate) rust_name: RustIdent,
    /// The exact bytes on the wire, which the Rust name may not match at all.
    pub(crate) wire_name: String,
}

/// Everything about one generated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeContract {
    rust_name: RustIdent,
    docs: Docs,
    kind: ContractKind,
    unknown_fields: UnknownFields,
    derives: Vec<Derive>,
    deser: DeserStrategy,
    origin: JsonPointer,
}

impl TypeContract {
    pub(crate) fn rust_name(&self) -> &RustIdent {
        &self.rust_name
    }

    pub(crate) fn docs(&self) -> &Docs {
        &self.docs
    }

    pub(crate) fn kind(&self) -> &ContractKind {
        &self.kind
    }

    pub(crate) fn unknown_fields(&self) -> UnknownFields {
        self.unknown_fields
    }

    pub(crate) fn derives(&self) -> &[Derive] {
        &self.derives
    }

    pub(crate) fn deser(&self) -> DeserStrategy {
        self.deser
    }

    pub(crate) fn origin(&self) -> &JsonPointer {
        &self.origin
    }
}

/// The frozen set of contracts a document produces.
#[derive(Debug, Default)]
pub(crate) struct Contracts {
    types: Vec<TypeContract>,
    /// The type each classified shape lowered to.
    ///
    /// The API model's only way in: an operation holds a `SchemaId`, the shape layer turns that
    /// into a [`ShapeKey`], and this says which Rust type the key became — after dedup, so a body
    /// and a component that classified alike name the one type that survived. Keeping it here
    /// rather than recomputing means the API model cannot disagree with the type layer about what
    /// a schema is.
    by_shape: BTreeMap<ShapeKey, TypeRef>,
    /// Optional-and-nullable collapses, still waiting for the position that says what each cost.
    collapses: Vec<Collapse>,
}

impl Contracts {
    pub(crate) fn types(&self) -> &[TypeContract] {
        &self.types
    }

    /// Whether any type holds an upload, and therefore whether the types module names
    /// `support::Upload`.
    pub(crate) fn uses_upload(&self) -> bool {
        self.types.iter().any(|contract| {
            finalize::references(contract.kind())
                .iter()
                .any(|ty| ty.holds_upload())
        })
    }

    pub(crate) fn get(&self, index: TypeIndex) -> Option<&TypeContract> {
        self.types.get(index.index())
    }

    /// The type a classified shape became, if it was reached at all.
    pub(crate) fn type_of(&self, key: &ShapeKey) -> Option<&TypeRef> {
        self.by_shape.get(key)
    }

    /// Every optional-and-nullable collapse, with the type it happened in.
    pub(crate) fn collapses(&self) -> &[Collapse] {
        &self.collapses
    }
}

/// Turn classified shapes into the frozen contracts a renderer reads.
///
/// # Errors
///
/// Returns a rejection when the caller's configuration asks, by name, for something a type cannot
/// be given. Everything the *document* gets wrong is a diagnostic; this is the one thing the
/// *caller* can get wrong badly enough to stop generation.
pub(crate) fn build(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    config: &Config,
    ctx: &mut Ctx,
) -> Result<Contracts, crate::diag::RejectError> {
    let mut lowered = lower::run(resolved, shapes, config, ctx);
    if let Some(conflict) = lowered.name_conflicts.first() {
        return Err(crate::diag::RejectError::new(
            crate::diag::RejectKind::UnsatisfiableConfig,
            format!(
                "the configuration names this type `{}`, but that identifier already belongs \
                 to another generated type; a configured name cannot be suffixed behind the \
                 caller's back, so pick one nothing else uses",
                conflict.chosen
            ),
        )
        .at(conflict.at.clone()));
    }
    // Validated against the shapes that were actually reserved, which is exactly the universe the
    // keyed lookups consulted during lowering — a key that matched nothing here matched nothing
    // anywhere, and honoring the configuration around it would be a silent no-op.
    let unmatched = config.unmatched_keys(|address| {
        lowered.types.iter().any(|provisional| {
            address.names(
                provisional.component.as_deref(),
                &provisional.origin.to_string(),
            )
        })
    });
    if let Some((key, table)) = unmatched.first() {
        return Err(crate::diag::RejectError::new(
            crate::diag::RejectKind::UnsatisfiableConfig,
            format!(
                "the configuration's `{table}` names `{key}`, which is neither a component name \
                 nor the JSON Pointer of any schema in this document"
            ),
        ));
    }
    let deduped = dedup::run(lowered.types, &mut lowered.by_shape, &mut lowered.collapses);
    let mut contracts = finalize::run(deduped, config, ctx)?;
    contracts.by_shape = lowered.by_shape;
    contracts.collapses = lowered.collapses;
    Ok(contracts)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{ContractKind, Contracts, TypeContract, TypeRef, build};
    use crate::config::Config;
    use crate::diag::{Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::{normalize, resolve, shape};

    pub(super) fn contracts_of(
        document: Value,
        config: &Config,
    ) -> eyre::Result<(Contracts, Vec<Diagnostic>)> {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = build(&resolved, &shapes, config, &mut ctx)?;
        Ok((contracts, ctx.into_diagnostics()))
    }

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

    pub(super) fn named<'a>(
        contracts: &'a Contracts,
        name: &str,
    ) -> eyre::Result<&'a TypeContract> {
        let found = contracts
            .types()
            .iter()
            .find(|contract| contract.rust_name().as_str() == name);
        found.ok_or_else(|| {
            let names: Vec<&str> = contracts
                .types()
                .iter()
                .map(|contract| contract.rust_name().as_str())
                .collect();
            eyre::eyre!("no type called {name}; there is {names:?}")
        })
    }

    pub(super) fn index_of(contracts: &Contracts, name: &str) -> eyre::Result<super::TypeIndex> {
        let position = contracts
            .types()
            .iter()
            .position(|contract| contract.rust_name().as_str() == name)
            .ok_or_else(|| eyre::eyre!("no type called {name}"))?;
        Ok(super::TypeIndex(u32::try_from(position)?))
    }

    pub(super) fn type_names(contracts: &Contracts) -> Vec<&str> {
        contracts
            .types()
            .iter()
            .map(|contract| contract.rust_name().as_str())
            .collect()
    }

    /// The one struct shape both halves of the dedup table are written against.
    fn same_shape() -> Value {
        json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}})
    }

    #[test_util::test]
    fn two_inline_shapes_that_behave_alike_become_one_type() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {
                    "type": "object",
                    "properties": {"left": same_shape(), "right": same_shape()},
                },
            })),
            &Config::default(),
        )?;
        // `Holder` plus one shared inline type, not two.
        assert_eq!(contracts.types().len(), 2);
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            panic!("expected a struct");
        };
        assert_eq!(fields[0].ty, fields[1].ty);
    }

    #[test_util::test]
    fn an_inline_shape_becomes_a_reference_to_the_type_the_document_named() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Pet": same_shape(),
                "Holder": {"type": "object", "properties": {"pet": same_shape()}},
            })),
            &Config::default(),
        )?;
        // Two types, not three: the anonymous twin became a reference.
        assert_eq!(contracts.types().len(), 2);
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            panic!("expected a struct");
        };
        let TypeRef::Option(inner) = &fields[0].ty else {
            panic!("expected an option");
        };
        assert_eq!(**inner, TypeRef::Named(index_of(&contracts, "Pet")?));
    }

    #[test_util::test]
    fn two_types_the_document_named_never_merge() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({"Cat": same_shape(), "Dog": same_shape()})),
            &Config::default(),
        )?;
        // Names are API. `Cat` and `Dog` look the same today and may not tomorrow.
        assert_eq!(type_names(&contracts), ["Cat", "Dog"]);
    }

    #[test_util::test]
    fn a_difference_no_reader_of_the_source_can_see_still_blocks_a_merge() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {
                    "type": "object",
                    "properties": {
                        "open": same_shape(),
                        // Identical fields; only the policy for undeclared members differs, which
                        // changes how the type *deserializes* and nothing about how it serializes.
                        "closed": {
                            "type": "object",
                            "required": ["a"],
                            "properties": {"a": {"type": "string"}},
                            "additionalProperties": false,
                        },
                    },
                },
            })),
            &Config::default(),
        )?;
        assert_eq!(contracts.types().len(), 3);
    }

    #[test_util::test]
    fn documentation_is_not_part_of_the_merge_key() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {
                    "type": "object",
                    "properties": {
                        "left": {"description": "the left one", "type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
                        "right": {"description": "the right one", "type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
                    },
                },
            })),
            &Config::default(),
        )?;
        assert_eq!(contracts.types().len(), 2);
    }

    #[test_util::test]
    fn a_name_the_caller_chose_is_a_declaration_of_identity() {
        let config = Config {
            names: [
                (
                    "/components/schemas/Holder/properties/left".to_owned(),
                    "Left".to_owned(),
                ),
                (
                    "/components/schemas/Holder/properties/right".to_owned(),
                    "Right".to_owned(),
                ),
            ]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {
                    "type": "object",
                    "properties": {"left": same_shape(), "right": same_shape()},
                },
            })),
            &config,
        )?;
        // Structurally identical, and named apart on purpose.
        assert_eq!(type_names(&contracts), ["Holder", "Left", "Right"]);
    }

    #[test_util::test]
    fn an_inline_shape_is_named_after_where_it_sits() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Pet": {
                    "type": "object",
                    "properties": {
                        "collar": {"type": "object", "properties": {"colour": {"type": "string"}}},
                        "toys": {
                            "type": "array",
                            "items": {"type": "object", "properties": {"name": {"type": "string"}}},
                        },
                    },
                },
            })),
            &Config::default(),
        )?;
        assert_eq!(type_names(&contracts), ["Pet", "PetCollar", "PetToysItem"]);
    }

    #[test_util::test]
    fn a_cycle_gets_exactly_one_box() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Node": {
                    "type": "object",
                    "required": ["next"],
                    "properties": {"next": {"$ref": "#/components/schemas/Node"}},
                },
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Node")?.kind() else {
            panic!("expected a struct");
        };
        assert!(
            matches!(fields[0].ty, TypeRef::Boxed(_)),
            "{:?}",
            fields[0].ty
        );
    }

    #[test_util::test]
    fn a_cycle_through_a_list_needs_no_box() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Node": {
                    "type": "object",
                    "required": ["children"],
                    "properties": {
                        "children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}},
                    },
                },
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Node")?.kind() else {
            panic!("expected a struct");
        };
        // A `Vec` already indirects, so boxing here would be a wart with no purpose.
        assert_eq!(
            fields[0].ty,
            TypeRef::Vec(Box::new(TypeRef::Named(index_of(&contracts, "Node")?)))
        );
    }

    #[test_util::test]
    fn a_recursive_alias_becomes_a_newtype_because_rust_has_no_recursive_aliases() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                // `type Tree = Vec<Tree>` is E0391 even though the `Vec` indirects.
                "Tree": {"type": "array", "items": {"$ref": "#/components/schemas/Tree"}},
            })),
            &Config::default(),
        )?;
        assert!(
            matches!(
                named(&contracts, "Tree")?.kind(),
                ContractKind::Newtype { .. }
            ),
            "{:?}",
            named(&contracts, "Tree")?.kind()
        );
    }

    #[test_util::test]
    fn a_plain_component_that_is_not_a_struct_is_a_name_for_the_shape() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({"Names": {"type": "array", "items": {"type": "string"}}})),
            &Config::default(),
        )?;
        assert_eq!(
            named(&contracts, "Names")?.kind(),
            &ContractKind::Alias {
                target: TypeRef::Vec(Box::new(TypeRef::String))
            }
        );
    }

    #[test_util::test]
    fn a_30_nullable_allof_reference_on_a_required_member_is_an_option() {
        // oxide `InstanceUpdate.auto_restart_policy`, in miniature: 3.0, required, and
        // `{nullable: true, allOf: [$ref]}` — the vendor documents `null` as the way to unset
        // the policy, so a type that cannot hold the null narrows the API.
        let mut ctx = Ctx::new();
        let document = serde_json::json!({
            "openapi": "3.0.3",
            "paths": {},
            "components": {"schemas": {
                "Policy": {"oneOf": [
                    {"description": "no", "type": "string", "enum": ["never"]},
                    {"description": "yes", "type": "string", "enum": ["best_effort"]},
                ]},
                "InstanceUpdate": {
                    "type": "object",
                    "required": ["policy"],
                    "properties": {
                        "policy": {
                            "nullable": true,
                            "description": "`null` unsets the policy.",
                            "allOf": [{"$ref": "#/components/schemas/Policy"}],
                        },
                    },
                },
            }},
        });
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = build(&resolved, &shapes, &Config::default(), &mut ctx)?;
        let ContractKind::Struct { fields } = named(&contracts, "InstanceUpdate")?.kind() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].presence, super::Presence::Nullable, "{fields:?}");
        assert!(
            matches!(&fields[0].ty, TypeRef::Option(inner) if matches!(inner.as_ref(), TypeRef::Named(_))),
            "{:?}",
            fields[0].ty
        );
    }

    #[test_util::test]
    fn a_configured_name_the_namer_cannot_honor_stops_generation() {
        let mut config = Config::default();
        config.names.insert("Widget".to_owned(), "Pet".to_owned());
        let result = contracts_of(
            with_schemas(json!({
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
                "Widget": {"type": "object", "properties": {"size": {"type": "integer"}}},
            })),
            &config,
        );
        // `Pet` claims its own name first; suffixing the configured name behind the caller's
        // back — or emitting `pub struct Pet` twice — are both worse than stopping.
        let Err(err) = result else {
            eyre::bail!("the collision is a rejection");
        };
        assert!(err.to_string().contains("already belongs"), "{err}");
    }

    #[test_util::test]
    fn a_configured_name_claims_first_and_a_later_component_steps_aside() {
        let mut config = Config::default();
        config.names.insert("AWidget".to_owned(), "Pet".to_owned());
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "AWidget": {"type": "object", "properties": {"size": {"type": "integer"}}},
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            })),
            &config,
        )?;
        named(&contracts, "Pet")?;
        named(&contracts, "Pet2")?;
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("taken already")),
            "{diagnostics:?}"
        );
    }

    #[test_util::test]
    fn an_inline_nullable_nameable_is_a_named_option_not_arbitrary_json() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {
                    "type": "object",
                    "required": ["data", "state", "either"],
                    "properties": {
                        "data": {
                            "type": ["object", "null"],
                            "required": ["id"],
                            "properties": {"id": {"type": "string"}},
                        },
                        "state": {"type": ["string", "null"], "enum": ["on", "off", null]},
                        "either": {"anyOf": [
                            {"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
                            {"type": "object", "required": ["b"], "properties": {"b": {"type": "string"}}},
                            {"type": "null"},
                        ]},
                    },
                },
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Holder")?.kind() else {
            panic!("expected a struct");
        };
        // Every member is an inline nullable *nameable* — a struct, a string enum, a union —
        // and each has to come out as an `Option` of a named type. Testing the unpeeled shape
        // in `names_needed` sent all three to `serde_json::Value`, silently.
        for field in fields {
            let TypeRef::Option(inner) = &field.ty else {
                panic!("`{}` is {:?}, not an Option", field.wire_name, field.ty);
            };
            assert!(
                matches!(inner.as_ref(), TypeRef::Named(_)),
                "`{}` holds {inner:?}, not a named type",
                field.wire_name
            );
        }
    }

    #[test_util::test]
    fn presence_records_all_four_cases_and_the_collapse_is_reported() {
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Thing": {
                    "type": "object",
                    "required": ["plain", "nullable"],
                    "properties": {
                        "plain": {"type": "string"},
                        "nullable": {"type": ["string", "null"]},
                        "optional": {"type": "string"},
                        "both": {"type": ["string", "null"]},
                    },
                },
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Thing")?.kind() else {
            panic!("expected a struct");
        };
        let presence: Vec<(&str, super::Presence, super::SkipRule)> = fields
            .iter()
            .map(|field| {
                (
                    field.wire_name.as_str(),
                    field.presence,
                    field.skip_serializing_if,
                )
            })
            .collect();
        assert_eq!(
            presence,
            [
                (
                    "both",
                    super::Presence::OptionalNullable,
                    super::SkipRule::WhenNone
                ),
                (
                    "nullable",
                    super::Presence::Nullable,
                    super::SkipRule::Never
                ),
                (
                    "optional",
                    super::Presence::Optional,
                    super::SkipRule::WhenNone
                ),
                ("plain", super::Presence::Required, super::SkipRule::Never),
            ]
        );
        // The collapse of the last case onto one `Option` is recorded rather than silent — and
        // recorded rather than *reported*, because what it costs depends on whether the type is on
        // the way in or on the way out, and nothing here knows a position. The API model turns
        // these into diagnostics ([`crate::api::presence`]); the contract's job is not to lose them.
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(contracts.collapses().len(), 1);
        assert_eq!(
            contracts.collapses()[0].at.to_string(),
            "/components/schemas/Thing/properties/both"
        );
    }

    #[test_util::test]
    fn a_default_that_cannot_be_a_value_of_the_field_is_dropped() {
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Thing": {
                    "type": "object",
                    "properties": {
                        "count": {"type": "integer", "default": "seven"},
                        "name": {"type": "string", "default": "unnamed"},
                    },
                },
            })),
            &Config::default(),
        )?;
        let ContractKind::Struct { fields } = named(&contracts, "Thing")?.kind() else {
            panic!("expected a struct");
        };
        assert_eq!(fields[0].wire_name, "count");
        assert_eq!(fields[0].default, None);
        assert_eq!(fields[1].default, Some(json!("unnamed")));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::InvalidDefault)
        );
    }

    #[test_util::test]
    fn a_typed_catch_all_becomes_a_flattened_member_and_captures() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Thing": {
                    "type": "object",
                    "properties": {"a": {"type": "string"}},
                    "additionalProperties": {"type": "integer"},
                },
            })),
            &Config::default(),
        )?;
        let contract = named(&contracts, "Thing")?;
        assert_eq!(
            contract.unknown_fields(),
            crate::config::UnknownFields::Capture
        );
        let ContractKind::Struct { fields } = contract.kind() else {
            panic!("expected a struct");
        };
        let extra = fields
            .last()
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(extra.flatten);
        assert_eq!(extra.ty, TypeRef::Map(Box::new(TypeRef::I64)));
    }

    #[test_util::test]
    fn a_derive_the_caller_asked_for_by_name_and_cannot_have_stops_generation() {
        let config = Config {
            type_derives: [(
                "Thing".to_owned(),
                [crate::config::Derive::Eq].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let document = with_schemas(json!({
            "Thing": {"type": "object", "properties": {"ratio": {"type": "number"}}},
        }));
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let error = build(&resolved, &shapes, &config, &mut ctx)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert_eq!(error.kind(), crate::RejectKind::UnsatisfiableConfig);
        assert!(error.detail().contains("Eq"), "{error}");
    }

    /// A key that names nothing is a typo, and a typo must not be a silent no-op.
    ///
    /// Before this refusal, `names = { "Peet" = "Pet" }` generated exactly as though the entry were
    /// not there: the caller asked for a rename and got nothing, with nothing saying so — the
    /// forbidden failure mode applied to the configuration instead of to the document. The message
    /// names the key and the map it sits in, because the caller's next step is to fix one of them.
    #[test_util::test]
    fn a_config_key_that_names_nothing_stops_generation() {
        let document = with_schemas(json!({
            "Thing": {"type": "object", "properties": {"ratio": {"type": "number"}}},
        }));
        let config = Config {
            names: [("Thingg".to_owned(), "Item".to_owned())]
                .into_iter()
                .collect(),
            ..Config::default()
        };
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let error = build(&resolved, &shapes, &config, &mut ctx)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert_eq!(error.kind(), crate::RejectKind::UnsatisfiableConfig);
        assert!(error.detail().contains("`Thingg`"), "{error}");
        assert!(error.detail().contains("names"), "{error}");
    }

    /// Both spellings of the one grammar reach the same type.
    #[test_util::test]
    fn a_pointer_key_and_a_name_key_reach_the_same_type() {
        let document = with_schemas(json!({
            "Thing": {"type": "object", "properties": {"name": {"type": "string"}}},
        }));
        for key in ["Thing", "/components/schemas/Thing"] {
            let config = Config {
                names: [(key.to_owned(), "Renamed".to_owned())]
                    .into_iter()
                    .collect(),
                ..Config::default()
            };
            let (contracts, _) = contracts_of(document.clone(), &config)?;
            assert!(
                contracts
                    .types()
                    .iter()
                    .any(|it| it.rust_name().as_str() == "Renamed"),
                "key `{key}` did not rename the type"
            );
        }
    }

    #[test_util::test]
    fn a_crate_wide_derive_a_type_cannot_have_is_skipped_and_reported() {
        let config = Config {
            derives: [crate::config::Derive::Eq].into_iter().collect(),
            ..Config::default()
        };
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Exact": {"type": "object", "properties": {"name": {"type": "string"}}},
                "Fuzzy": {"type": "object", "properties": {"ratio": {"type": "number"}}},
            })),
            &config,
        )?;
        assert!(
            named(&contracts, "Exact")?
                .derives()
                .contains(&crate::config::Derive::Eq)
        );
        assert!(
            !named(&contracts, "Fuzzy")?
                .derives()
                .contains(&crate::config::Derive::Eq)
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.class() == crate::BreakageClass::UnsatisfiableDerive)
        );
    }

    #[test_util::test]
    fn default_is_configurable_and_propagates_only_through_defaultable_members() {
        let config = Config {
            derives: [crate::config::Derive::Default].into_iter().collect(),
            ..Config::default()
        };
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Leaf": {"type": "object", "properties": {"name": {"type": "string"}}},
                "Choice": {"type": "string", "enum": ["a", "b"]},
                "Good": {"type": "object", "properties": {
                    "leaf": {"$ref": "#/components/schemas/Leaf"}
                }},
                "Blocked": {"type": "object", "properties": {
                    "choice": {"$ref": "#/components/schemas/Choice"}
                }},
                "Address": {"type": "string", "format": "ip"},
                "BlockedAddress": {"type": "object", "properties": {
                    "address": {"$ref": "#/components/schemas/Address"}
                }},
            })),
            &config,
        )?;

        for name in ["Leaf", "Good"] {
            assert!(
                named(&contracts, name)?
                    .derives()
                    .contains(&crate::config::Derive::Default),
                "{name} should carry `Default`"
            );
        }
        for name in ["Choice", "Blocked", "Address", "BlockedAddress"] {
            assert!(
                !named(&contracts, name)?
                    .derives()
                    .contains(&crate::config::Derive::Default),
                "{name} cannot carry `Default`"
            );
        }
    }

    #[test_util::test]
    fn a_component_becomes_a_struct_named_after_itself() {
        let (contracts, diagnostics) = contracts_of(
            with_schemas(json!({
                "Pet": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": {"type": "string"},
                        "tag": {"type": "string"},
                    },
                },
            })),
            &Config::default(),
        )?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(type_names(&contracts), ["Pet"]);
        let ContractKind::Struct { fields } = named(&contracts, "Pet")?.kind() else {
            panic!("expected a struct");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].wire_name, "name");
        assert_eq!(fields[0].rust_name.as_str(), "name");
    }
}
