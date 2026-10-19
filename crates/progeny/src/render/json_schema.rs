//! `schemars::JsonSchema` implementations, written from the wire contract.
//!
//! Written by hand for the reason [`crate::config::Derive`] is closed: the schemars derive learns
//! the wire format from `#[serde(...)]` attributes, and a type on the hand-written serde path
//! carries none, so a derived schema would name members by their Rust names, require what the
//! wire leaves optional, and tag unions the way no payload does. Rendered from the same record the
//! serde impls are rendered from, the schema is a third reading of one contract and cannot
//! disagree with the other two — under either serde strategy, because it consults neither.
//!
//! **What a schema says.** A member is named by its wire name, required when the description
//! requires its key, and nullable when the description allows `null`. A struct's undeclared
//! members are its `additionalProperties`: the type of a declared catch-all, anything for the
//! members a read form keeps in `extra`, and `false` where the description forbids them. A union
//! is an `anyOf` of its variants as the wire carries them — a consumed tag added into each
//! variant's own schema, a carried one pinned beside it — and every open enum ends in its
//! fallback, which accepts what the fallback variant keeps. A read form is the well-formed payload: every member
//! optional, typed as declared. The lenient decoder tolerates more than that and reports it; the
//! schema describes what reads without a report.
//!
//! **What it names.** Only the generated types it holds, through `subschema_for`, so each is one
//! definition however often it is used and a recursive type is a `$ref` to itself. Everything else
//! — scalars, formats, lists, maps, the support types — is written inline from the contract, which
//! is why no schemars feature for `chrono`, `uuid` or `indexmap` is needed, and an alias is written
//! as what it names, since an alias cannot carry an impl.

use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::quote;
use serde_json::Value;

use super::types::{Scope, type_ref_in};
use crate::config::{Config, UnknownFields};
use crate::contract::{
    Capture, ContractKind, Contracts, FieldContract, Gate, Presence, StringVariant, TagRoutes,
    TaggedVariant, TypeContract, TypeIndex, TypeRef, VariantContract,
};
use crate::shape::{Docs, Format};

/// A schema under construction: literal JSON where the contract fixes it, and calls into the
/// generator where it names another generated type.
enum Node {
    /// A subtree the contract fixes completely.
    Json(Value),
    /// An object whose members include computed schemas.
    Object(Vec<(String, Node)>),
    Array(Vec<Node>),
    /// `generator.subschema_for::<T>()`: a `$ref` to `T`'s one definition.
    Reference(TokenStream),
    /// A variant of a union whose tag is consumed: the payload type's own schema, written in
    /// place, with the tag member added to it.
    ///
    /// Added *into* the payload's schema rather than beside it in an `allOf`, because a payload
    /// that forbids undeclared members would then forbid the tag too: serde consumes the tag
    /// before the payload sees it, and a validator does not.
    Tagged {
        ty: TokenStream,
        tag: String,
        value: String,
    },
    /// A computed schema with keywords added beside it.
    Annotated(Box<Node>, Vec<(String, Value)>),
}

impl Node {
    fn object(members: impl IntoIterator<Item = (&'static str, Node)>) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(key, node)| (key.to_owned(), node))
                .collect(),
        )
    }

    fn json(value: Value) -> Self {
        Self::Json(value)
    }

    /// The schema with keywords added, merged into it when it is an object already.
    fn annotated(self, keywords: Vec<(String, Value)>) -> Self {
        if keywords.is_empty() {
            return self;
        }
        match self {
            Self::Json(Value::Object(mut object)) => {
                object.extend(keywords);
                Self::Json(Value::Object(object))
            }
            Self::Object(mut members) => {
                members.extend(
                    keywords
                        .into_iter()
                        .map(|(key, value)| (key, Self::Json(value))),
                );
                Self::Object(members)
            }
            other => Self::Annotated(Box::new(other), keywords),
        }
    }

    /// Whether rendering this calls into the generator.
    fn uses_generator(&self) -> bool {
        match self {
            Self::Json(_) => false,
            Self::Reference(_) | Self::Tagged { .. } => true,
            Self::Object(members) => members.iter().any(|(_, node)| node.uses_generator()),
            Self::Array(items) => items.iter().any(Self::uses_generator),
            Self::Annotated(inner, _) => inner.uses_generator(),
        }
    }
}

/// The `JsonSchema` impl of one type, or nothing when the configuration asks for none.
pub(super) fn implementation(
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let Some(gate) = contract.json_schema() else {
        return TokenStream::new();
    };
    let writer = Writer {
        contracts,
        config,
        scope: Scope::of(contract),
    };
    let Some(schema) = writer.contract(contract) else {
        return TokenStream::new();
    };
    // A schema is an object at the top: a type whose whole schema is another type's `$ref` wraps
    // it, because `json_schema` must not return a bare reference.
    let schema = match schema {
        Node::Json(Value::Object(_)) | Node::Object(_) => schema,
        other => Node::object([("allOf", Node::Array(vec![other]))]),
    }
    .annotated(doc_keywords(contract.docs()));
    let generator = if schema.uses_generator() {
        quote! { generator }
    } else {
        quote! { _generator }
    };
    let body = top_level(&schema);
    let name = super::serde_impl::implementation_type(contract);
    let rust_name = contract.rust_name().as_str();
    let schema_name = if contract.form().is_lenient() {
        format!("read.{rust_name}")
    } else {
        rust_name.to_owned()
    };
    let schema_id = format!("::{rust_name}");
    let gate = match gate {
        Gate::Always => TokenStream::new(),
        Gate::Feature(feature) => quote! { #[cfg(feature = #feature)] },
    };
    quote! {
        #gate
        impl ::schemars::JsonSchema for #name {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(#schema_name)
            }

            fn schema_id() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(::core::concat!(::core::module_path!(), #schema_id))
            }

            fn json_schema(#generator: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                #body
            }
        }
    }
}

/// Every generated type the impl of `contract` names: what it references, with each alias
/// replaced by what the alias names.
pub(super) fn named(contract: &TypeContract, contracts: &Contracts) -> Vec<TypeIndex> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for ty in contract.kind().references() {
        ty.named(&mut pending);
    }
    let mut seen = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if !seen.insert(index) {
            continue;
        }
        match contracts.get(index).map(TypeContract::kind) {
            Some(ContractKind::Alias { target }) => target.named(&mut pending),
            Some(_) => out.push(index),
            None => {}
        }
    }
    out
}

struct Writer<'a> {
    contracts: &'a Contracts,
    config: &'a Config,
    scope: Scope,
}

impl Writer<'_> {
    /// A type's own schema, or `None` for an alias, which has no item to implement it on.
    fn contract(&self, contract: &TypeContract) -> Option<Node> {
        Some(match contract.kind() {
            ContractKind::Struct { fields } => self.structure(contract, fields),
            ContractKind::Enum { variants, .. } => self.untagged(variants),
            ContractKind::TaggedEnum { tag, variants, .. } => self.tagged(tag, variants),
            ContractKind::CarriedTagEnum {
                tag,
                variants,
                routes,
                ..
            } => self.carried(tag, variants, *routes),
            ContractKind::KeyedEnum { variants, .. } => self.keyed(variants),
            ContractKind::StringEnum {
                variants,
                any_other,
                ..
            } => strings(variants, *any_other),
            ContractKind::Newtype { inner } => self.reference(inner),
            ContractKind::Tuple { items } => self.tuple(items),
            ContractKind::Alias { .. } => return None,
        })
    }

    fn structure(&self, contract: &TypeContract, fields: &[FieldContract]) -> Node {
        // A read form requires nothing: every member of it is optional, which is the point of it.
        let lenient = contract.form().is_lenient();
        let mut properties = Vec::new();
        let mut required = Vec::new();
        let mut additional = None;
        let mut flattened = None;
        for field in fields {
            // The union beside the members holds the rest, as the document's own `allOf` of the
            // two would say.
            if field.capture == Some(Capture::Union) {
                flattened = Some(self.reference(&field.ty));
                continue;
            }
            if field.is_capture() {
                // The capture member is flattened: it is the members no declared field claims, so
                // its element type is what an undeclared member may be.
                additional = Some(match &field.ty {
                    TypeRef::Map(element) if **element == TypeRef::Value => {
                        Node::json(Value::Bool(true))
                    }
                    TypeRef::Map(element) => self.reference(element),
                    other => self.reference(other),
                });
                continue;
            }
            if !lenient && matches!(field.presence, Presence::Required | Presence::Nullable) {
                required.push(Value::String(field.wire_name.clone()));
            }
            let mut keywords = doc_keywords(&field.docs);
            if let Some(default) = field.default.as_ref().and_then(literal) {
                keywords.push(("default".to_owned(), default));
            }
            properties.push((
                field.wire_name.clone(),
                self.reference(member_type(field, lenient))
                    .annotated(keywords),
            ));
        }
        // Without a capture member, an undeclared member is either refused or read and dropped.
        // Dropped is what JSON Schema assumes without being told; refused it has to be told. A
        // read form never refuses one, and keeps it in `extra` instead.
        if additional.is_none() && contract.unknown_fields() == UnknownFields::Deny && !lenient {
            additional = Some(Node::json(Value::Bool(false)));
        }
        let mut members = vec![("type", Node::json(Value::from("object")))];
        if !properties.is_empty() {
            members.push(("properties", Node::Object(properties)));
        }
        if !required.is_empty() {
            members.push(("required", Node::json(Value::Array(required))));
        }
        if let Some(additional) = additional {
            members.push(("additionalProperties", additional));
        }
        if let Some(flattened) = flattened {
            members.push(("allOf", Node::Array(vec![flattened])));
        }
        Node::object(members)
    }

    /// A union matched by shape: whichever variant reads the payload, and the fallback after them.
    fn untagged(&self, variants: &[VariantContract]) -> Node {
        let mut arms: Vec<Node> = variants
            .iter()
            .map(|variant| self.reference(&variant.ty))
            .collect();
        arms.push(fallback("fits no variant's shape"));
        Node::object([("anyOf", Node::Array(arms))])
    }

    /// A union whose tag member is consumed: each variant's payload carries the tag beside its own
    /// members, as the one value naming that variant.
    fn tagged(&self, tag: &str, variants: &[TaggedVariant]) -> Node {
        let mut arms: Vec<Node> = variants
            .iter()
            .map(|variant| self.tagged_variant(tag, variant))
            .collect();
        arms.push(fallback("names no variant in its tag"));
        Node::object([("anyOf", Node::Array(arms))])
    }

    fn tagged_variant(&self, tag: &str, variant: &TaggedVariant) -> Node {
        let tag_only = || tag_member(tag, &variant.tag_value);
        let payload = match &variant.ty {
            TypeRef::Boxed(inner) => inner.as_ref(),
            other => other,
        };
        match payload {
            TypeRef::Named(index)
                if self.contracts.get(*index).is_some_and(|contract| {
                    !matches!(contract.kind(), ContractKind::Alias { .. })
                }) =>
            {
                Node::Tagged {
                    ty: self.path(*index),
                    tag: tag.to_owned(),
                    value: variant.tag_value.clone(),
                }
            }
            TypeRef::Unit => Node::json(tag_only()),
            other => Node::object([(
                "allOf",
                Node::Array(vec![Node::json(tag_only()), self.reference(other)]),
            )]),
        }
    }

    /// A union whose variants carry the tag member themselves: each variant's own schema, with
    /// the one tag value that picks it.
    ///
    /// Beside the variant's schema rather than inside it, because the variant declares the tag
    /// member itself, so even one that forbids undeclared members accepts it.
    fn carried(&self, tag: &str, variants: &[TaggedVariant], routes: TagRoutes) -> Node {
        let mut arms: Vec<Node> = variants
            .iter()
            .enumerate()
            .map(|(index, variant)| {
                let member = carried_tag_member(
                    tag,
                    variant,
                    routes.absent == Some(index),
                    routes.null == Some(index),
                );
                Node::object([(
                    "allOf",
                    Node::Array(vec![Node::json(member), self.reference(&variant.ty)]),
                )])
            })
            .collect();
        arms.push(fallback("names no variant in its tag"));
        Node::object([("anyOf", Node::Array(arms))])
    }

    /// A union named on the wire: the listed strings, one single-member object per data variant,
    /// and the open arm.
    fn keyed(&self, variants: &[crate::contract::KeyedVariant]) -> Node {
        let units: Vec<Value> = variants
            .iter()
            .filter(|variant| variant.payload.is_none())
            .map(|variant| Value::String(variant.wire_name.clone()))
            .collect();
        let mut arms = Vec::new();
        if !units.is_empty() {
            arms.push(Node::json(
                serde_json::json!({"type": "string", "enum": units}),
            ));
        }
        for variant in variants {
            let Some(payload) = &variant.payload else {
                continue;
            };
            arms.push(Node::object([
                ("type", Node::json(Value::from("object"))),
                (
                    "properties",
                    Node::Object(vec![(variant.wire_name.clone(), self.reference(payload))]),
                ),
                (
                    "required",
                    Node::json(serde_json::json!([variant.wire_name])),
                ),
                ("additionalProperties", Node::json(Value::Bool(false))),
            ]));
        }
        arms.push(fallback("names no variant"));
        Node::object([("anyOf", Node::Array(arms))])
    }

    fn tuple(&self, items: &[TypeRef]) -> Node {
        let arity = Value::from(items.len());
        Node::object([
            ("type", Node::json(Value::from("array"))),
            (
                "prefixItems",
                Node::Array(items.iter().map(|item| self.reference(item)).collect()),
            ),
            ("items", Node::json(Value::Bool(false))),
            ("minItems", Node::json(arity.clone())),
            ("maxItems", Node::json(arity)),
        ])
    }

    /// The schema of a type reference anywhere but at the top of a member, where an `Option`
    /// means `null`.
    fn reference(&self, ty: &TypeRef) -> Node {
        match ty {
            TypeRef::Named(index) => match self.contracts.get(*index) {
                Some(contract) => match contract.kind() {
                    // An alias has no impl of its own: what it names is written in its place.
                    ContractKind::Alias { target } => self
                        .reference(target)
                        .annotated(doc_keywords(contract.docs())),
                    _ => Node::Reference(self.path(*index)),
                },
                // Unreachable: every index comes from the contract set it is rendered against.
                None => Node::json(Value::Object(serde_json::Map::new())),
            },
            TypeRef::Unit => Node::json(serde_json::json!({"type": "null"})),
            TypeRef::Bool => Node::json(serde_json::json!({"type": "boolean"})),
            TypeRef::Integer(integer) => Node::json(integer_schema(*integer)),
            TypeRef::F64 => Node::json(serde_json::json!({"type": "number", "format": "double"})),
            TypeRef::String => Node::json(serde_json::json!({"type": "string"})),
            TypeRef::Format(format) => Node::json(format_schema(*format)),
            // A file part of a multipart body: bytes, never JSON.
            TypeRef::Upload => Node::json(serde_json::json!({
                "type": "string",
                "contentMediaType": "application/octet-stream",
            })),
            TypeRef::Value => Node::json(Value::Object(serde_json::Map::new())),
            TypeRef::Option(inner) | TypeRef::Presence(inner) => Node::object([(
                "anyOf",
                Node::Array(vec![
                    self.reference(inner),
                    Node::json(serde_json::json!({"type": "null"})),
                ]),
            )]),
            TypeRef::Vec(inner) => Node::object([
                ("type", Node::json(Value::from("array"))),
                ("items", self.reference(inner)),
            ]),
            // Every map is keyed by `String`, whichever map type the configuration chose.
            TypeRef::Map(inner) => Node::object([
                ("type", Node::json(Value::from("object"))),
                ("additionalProperties", self.reference(inner)),
            ]),
            TypeRef::Array(inner, length) => Node::object([
                ("type", Node::json(Value::from("array"))),
                ("items", self.reference(inner)),
                ("minItems", Node::json(Value::from(*length))),
                ("maxItems", Node::json(Value::from(*length))),
            ]),
            TypeRef::Tuple(items) => self.tuple(items),
            TypeRef::Boxed(inner) => self.reference(inner),
        }
    }

    /// A generated type, spelled from the module the impl is written in.
    fn path(&self, index: TypeIndex) -> TokenStream {
        type_ref_in(
            &TypeRef::Named(index),
            self.contracts,
            self.config,
            self.scope,
        )
    }
}

/// The type a member's schema describes, with the `Option` that only encodes an absent key taken
/// off.
///
/// An optional member is `Option<T>` and leaves `None` off the wire, so `null` is not one of its
/// values; a read form's required member was made optional the same way. Every other `Option` at
/// the top of a member is the description's own nullability, kept: a nullable member, required or
/// not, and the three-state `Presence`.
fn member_type(field: &FieldContract, lenient: bool) -> &TypeRef {
    match (&field.ty, field.presence) {
        (TypeRef::Option(inner), Presence::Optional) => inner,
        (TypeRef::Option(inner), Presence::Required) if lenient => inner,
        (ty, _) => ty,
    }
}

/// An object whose tag member holds exactly `value`.
fn tag_member(tag: &str, value: &str) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {tag: {"const": value}},
        "required": [tag],
    })
}

/// The tag member of one variant of a carried-tag union: every spelling that names it, and the
/// absent or `null` member where that names it too.
fn carried_tag_member(tag: &str, variant: &TaggedVariant, absent: bool, null: bool) -> Value {
    let mut values: Vec<Value> = variant
        .spellings()
        .map(|spelling| Value::String(spelling.to_owned()))
        .collect();
    if null {
        values.push(Value::Null);
    }
    let constraint = match values.as_slice() {
        [one] => serde_json::json!({"const": one}),
        _ => serde_json::json!({"enum": values}),
    };
    if absent {
        serde_json::json!({"type": "object", "properties": {tag: constraint}})
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {tag: constraint},
            "required": [tag],
        })
    }
}

/// An open enum of strings: the values the description lists, and any other string, which the
/// fallback variant keeps.
fn strings(variants: &[StringVariant], any_other: bool) -> Node {
    let listed: Vec<Value> = variants
        .iter()
        .map(|variant| Value::String(variant.wire_name.clone()))
        .collect();
    let unlisted = serde_json::json!({
        "description": if any_other {
            "Any other value, which the description allows beside the ones it names."
        } else {
            "A value the description does not list, kept exactly as it arrived."
        },
    });
    let arms = if listed.is_empty() {
        vec![unlisted]
    } else {
        vec![serde_json::json!({"enum": listed}), unlisted]
    };
    Node::json(serde_json::json!({"type": "string", "anyOf": arms}))
}

/// The open arm of a union, which keeps any payload.
fn fallback(when: &str) -> Node {
    Node::json(serde_json::json!({
        "description": format!("A payload that {when}, kept exactly as it arrived."),
    }))
}

/// An integer's schema: its format, and the bounds of a width narrower than 64 bits.
///
/// An unsigned integer says its zero lower bound at every width, and a 64-bit one says no upper
/// bound, which is what its format already implies.
fn integer_schema(integer: crate::shape::IntType) -> Value {
    use crate::shape::IntType;

    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), Value::from("integer"));
    schema.insert("format".to_owned(), Value::from(integer.format()));
    let (minimum, maximum) = integer.range();
    if integer != IntType::I64 {
        schema.insert("minimum".to_owned(), serde_json::json!(minimum));
    }
    if !matches!(integer, IntType::I64 | IntType::U64) {
        schema.insert("maximum".to_owned(), serde_json::json!(maximum));
    }
    Value::Object(schema)
}

/// A format's schema: the string the wire carries, with the format the description named.
///
/// The same whichever crate the configuration renders it as, because the wire text is the same.
fn format_schema(format: Format) -> Value {
    let named = |name: &str| serde_json::json!({"type": "string", "format": name});
    match format {
        Format::DateTime => named("date-time"),
        Format::Date => named("date"),
        Format::Time => named("time"),
        Format::Uuid => named("uuid"),
        Format::Ipv4 => named("ipv4"),
        Format::Ipv6 => named("ipv6"),
        Format::Ip => serde_json::json!({
            "type": "string",
            "anyOf": [{"format": "ipv4"}, {"format": "ipv6"}],
        }),
        Format::Base64 => serde_json::json!({"type": "string", "contentEncoding": "base64"}),
        // A binary string inside JSON is text the generated type holds unparsed.
        Format::Binary => serde_json::json!({"type": "string"}),
    }
}

/// The annotations a type or member's documentation contributes.
fn doc_keywords(docs: &Docs) -> Vec<(String, Value)> {
    let mut keywords = Vec::new();
    if let Some(title) = &docs.title {
        keywords.push(("title".to_owned(), Value::String(title.clone())));
    }
    if let Some(description) = &docs.description {
        keywords.push(("description".to_owned(), Value::String(description.clone())));
    }
    if docs.deprecated {
        keywords.push(("deprecated".to_owned(), Value::Bool(true)));
    }
    keywords
}

/// A value the generated source can spell exactly, or `None`.
///
/// A number that is no `u64`, `i64` or finite `f64` — an arbitrary-precision literal the model
/// keeps byte-exact — has no Rust literal, and a `default` is an annotation, so it is left out
/// rather than rounded.
fn literal(value: &Value) -> Option<Value> {
    match value {
        Value::Number(number) => {
            if number.as_u64().is_some() || number.as_i64().is_some() {
                Some(value.clone())
            } else {
                number
                    .as_f64()
                    .filter(|float| float.is_finite())
                    .map(Value::from)
            }
        }
        Value::Array(items) => items
            .iter()
            .map(literal)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        Value::Object(members) => members
            .iter()
            .map(|(key, member)| literal(member).map(|member| (key.clone(), member)))
            .collect::<Option<serde_json::Map<_, _>>>()
            .map(Value::Object),
        Value::Null | Value::Bool(_) | Value::String(_) => Some(value.clone()),
    }
}

/// The body of `json_schema`: the schema object, built member by member.
///
/// Not through `json!`: that macro recurses once per member, and a struct of a few hundred members
/// — the corpus has them — runs past the compiler's recursion limit in the consumer's build.
fn top_level(schema: &Node) -> TokenStream {
    match schema {
        Node::Json(Value::Object(members)) => {
            let map = json_map(members.iter().map(|(key, value)| (key, json(value))));
            quote! { ::schemars::Schema::from(#map) }
        }
        Node::Object(members) => {
            let map = json_map(members.iter().map(|(key, node)| (key, value(node))));
            quote! { ::schemars::Schema::from(#map) }
        }
        // Unreachable: the caller wraps every other node in an object first.
        other => {
            let value = value(other);
            quote! {
                ::schemars::Schema::from(::serde_json::Map::from_iter([(
                    ::std::string::String::from("allOf"),
                    ::serde_json::Value::Array(::std::vec::Vec::from([#value])),
                )]))
            }
        }
    }
}

/// An expression of type `serde_json::Value` for a node.
fn value(node: &Node) -> TokenStream {
    match node {
        Node::Json(literal) => json(literal),
        Node::Object(members) => {
            let map = json_map(members.iter().map(|(key, node)| (key, value(node))));
            quote! { ::serde_json::Value::Object(#map) }
        }
        Node::Array(items) => {
            let items = items.iter().map(value);
            quote! { ::serde_json::Value::Array(::std::vec::Vec::from([#(#items),*])) }
        }
        Node::Reference(ty) => {
            quote! { ::serde_json::Value::from(generator.subschema_for::<#ty>()) }
        }
        Node::Tagged {
            ty,
            tag,
            value: tag_value,
        } => {
            quote! {
                {
                    let mut variant = <#ty as ::schemars::JsonSchema>::json_schema(generator);
                    let object = variant.ensure_object();
                    if let ::serde_json::Value::Object(properties) = object
                        .entry("properties")
                        .or_insert_with(|| ::serde_json::Value::Object(::serde_json::Map::new()))
                    {
                        properties.insert(
                            ::std::string::String::from(#tag),
                            ::serde_json::Value::Object(::serde_json::Map::from_iter([(
                                ::std::string::String::from("const"),
                                ::serde_json::Value::from(#tag_value),
                            )])),
                        );
                    }
                    if let ::serde_json::Value::Array(required) = object
                        .entry("required")
                        .or_insert_with(|| ::serde_json::Value::Array(::std::vec::Vec::new()))
                    {
                        required.push(::serde_json::Value::from(#tag));
                    }
                    ::serde_json::Value::from(variant)
                }
            }
        }
        Node::Annotated(inner, keywords) => {
            let inner = value(inner);
            let inserts = keywords.iter().map(|(key, keyword)| {
                let keyword = json(keyword);
                quote! { object.insert(::std::string::String::from(#key), #keyword); }
            });
            quote! {
                {
                    let mut schema = #inner;
                    if let ::serde_json::Value::Object(object) = &mut schema {
                        #(#inserts)*
                    }
                    schema
                }
            }
        }
    }
}

/// A `serde_json::Map` from member names and value expressions.
fn json_map<'a>(members: impl Iterator<Item = (&'a String, TokenStream)>) -> TokenStream {
    let members: Vec<TokenStream> = members
        .map(|(key, value)| quote! { (::std::string::String::from(#key), #value) })
        .collect();
    if members.is_empty() {
        return quote! { ::serde_json::Map::new() };
    }
    quote! { ::serde_json::Map::from_iter([#(#members),*]) }
}

/// An expression of type `serde_json::Value` for a literal.
fn json(value: &Value) -> TokenStream {
    match value {
        Value::Null => quote! { ::serde_json::Value::Null },
        Value::Bool(flag) => quote! { ::serde_json::Value::Bool(#flag) },
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                quote! { ::serde_json::Value::from(#unsigned) }
            } else if let Some(signed) = number.as_i64() {
                quote! { ::serde_json::Value::from(#signed) }
            } else {
                // `literal` let through only numbers with a finite `f64` reading.
                let float = number.as_f64().unwrap_or_default();
                quote! { ::serde_json::Value::from(#float) }
            }
        }
        Value::String(text) => quote! { ::serde_json::Value::from(#text) },
        Value::Array(items) => {
            let items = items.iter().map(json);
            quote! { ::serde_json::Value::Array(::std::vec::Vec::from([#(#items),*])) }
        }
        Value::Object(members) => {
            let map = json_map(members.iter().map(|(key, member)| (key, json(member))));
            quote! { ::serde_json::Value::Object(#map) }
        }
    }
}
