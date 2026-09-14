//! Rendering types: a direct transcription of the contract records.
//!
//! **This module makes no decisions.** Every question it could ask — what a field is called on the
//! wire, whether an absent key is legal, which derives appear, how long a tuple is — was answered in
//! [`crate::contract`] and is sitting in the record it is handed. When the derive strategy is
//! selected the same record renders as `#[serde(...)]` attributes; when the hand-written strategy is
//! selected it renders as function bodies instead. Two renderings of one record, which is the whole
//! discipline in one sentence.

use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::api::{ApiModel, BodyContract, ResponseBody};
use crate::config::{BytesRepr, Config, DateTimeCrate, MapKind, UuidCrate};
use crate::contract::{
    ContractKind, Contracts, DeserStrategy, FieldContract, Form, RustIdent, SkipRule, TypeContract,
    TypeRef,
};
use crate::shape::{Docs, Format};

/// Where a type is being spelled from, which decides how a named type and the support module are
/// reached.
///
/// The type layer is two modules: the root, holding strict and shared types, and `read`, holding
/// the read forms beside re-exports of every shared type a response can yield. From inside
/// `read` every type is a bare name; from the root a read form is `read::Name`; from an edge
/// module both go through `super::types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The root of the types module.
    Types,
    /// Inside `types::read`.
    Read,
    /// A sibling module of `types`: the client or the server.
    Edge,
}

impl Scope {
    /// The module a contract's own items are rendered in.
    pub(super) fn of(contract: &TypeContract) -> Self {
        if contract.form().is_lenient() {
            Self::Read
        } else {
            Self::Types
        }
    }

    /// The path to the support module from here.
    pub(super) fn support(self) -> TokenStream {
        match self {
            Self::Types | Self::Edge => quote! { super::support },
            Self::Read => quote! { super::super::support },
        }
    }
}

/// The rendered type layer: the root module's items and, when a response yields anything, the
/// `read` module's.
pub(super) struct Rendered {
    pub(super) root: TokenStream,
    pub(super) read: Option<TokenStream>,
}

/// Render every type in the contract set, with every impl each one carries.
pub(super) fn render(
    contracts: &Contracts,
    api: &ApiModel,
    api_modules: bool,
    external_api_modules: bool,
    config: &Config,
) -> Rendered {
    let (root_needed, read_needed) = deprecated_alias_names(contracts, api, api_modules);
    let deprecated =
        deprecated_aliases(contracts, &root_needed, Scope::Types, external_api_modules);
    let mut root = Vec::new();
    let mut read = Vec::new();
    for contract in contracts.types() {
        let item = one(contract, contracts, config);
        let serde = super::serde_impl::one(contract);
        let lenient = super::lenient::impls(contract, contracts, config);
        let tokens = quote! { #item #serde #lenient };
        if contract.form().is_lenient() {
            read.push(tokens);
        } else {
            root.push(tokens);
        }
    }
    let read = contracts.has_read_forms().then(|| {
        let deprecated =
            deprecated_aliases(contracts, &read_needed, Scope::Read, external_api_modules);
        let shared = contracts
            .types()
            .iter()
            .filter(|contract| contract.form() == Form::Shared)
            .map(|contract| {
                let name = ident(contract.rust_name());
                if contract.docs().deprecated {
                    quote! {
                        #[expect(
                            deprecated,
                            reason = "the read module re-exports every type a response yields"
                        )]
                        pub use super::#name;
                    }
                } else {
                    quote! { pub use super::#name; }
                }
            });
        quote! {
            /// What a response decodes into.
            ///
            /// Every type here is the *read form* of a type in the description. A struct the
            /// description requires members of has every member optional here, and keeps the
            /// members the description does not declare in `extra`. A type the description
            /// reaches from a request as well has its strict form in the parent module and its
            /// read twin here, and converts into it with `From`; a type both forms agree on —
            /// nothing required anywhere inside — is defined in the parent module and
            /// re-exported here, so everything a response yields can be named as `read::…`.
            /// Such a shared type has no `extra`: an undeclared member it reads is reported and
            /// not kept, because the same type is what a request sends.
            ///
            /// Reading through the generated client tolerates a member that is absent, `null`
            /// or unreadable and reports it, and leaves out a list element it cannot read.
            /// `serde_json::from_str` on a read struct does the same and drops the report; keep
            /// the report with `Decoded::from_json`, which reads any of these types and every
            /// container of them. On a read alias of a list `serde_json::from_str` is serde's
            /// own list reading, which refuses the whole list over one element.
            ///
            /// Every type here carries the sites a report keys its entries at: `Pet::SITE` for
            /// what the type itself tolerated, `Pet::SITE_NAME` for one of its members.
            pub mod read {
                #deprecated
                #(#shared)*
                #(#read)*
            }
        }
    });
    Rendered {
        root: quote! { #deprecated #(#root)* },
        read,
    }
}

/// Non-deprecated paths the generated implementation uses to name deprecated public contracts.
///
/// The public declaration keeps its `#[deprecated]` marker, so callers still get the intended
/// warning. Generated derives and support code use these transparent aliases instead: procedural
/// macro expansions cannot inherit a field's `#[expect]`, so aliases are the only way to keep those
/// internal uses clean without a broad `#[allow(deprecated)]`. Each module of the type layer
/// carries its own, aliasing what that module's items name.
fn deprecated_aliases(
    contracts: &Contracts,
    needed: &BTreeSet<String>,
    scope: Scope,
    external_api_modules: bool,
) -> TokenStream {
    let visibility = if external_api_modules {
        quote! { pub }
    } else {
        quote! { pub(crate) }
    };
    let aliases: Vec<TokenStream> = contracts
        .types()
        .iter()
        .filter(|contract| {
            contract.docs().deprecated
                && needed.contains(contract.rust_name().as_str())
                && (Scope::of(contract) == scope
                    || (scope == Scope::Read && contract.form() == Form::Shared))
        })
        .map(|contract| {
            let name = ident(contract.rust_name());
            quote! {
                #[expect(
                    deprecated,
                    reason = "generated internals need a non-deprecated path to this public contract"
                )]
                #visibility type #name = super::#name;
            }
        })
        .collect();
    if aliases.is_empty() {
        return TokenStream::new();
    }
    quote! {
        #[doc(hidden)]
        #visibility mod __progeny_deprecated {
            #(#aliases)*
        }
    }
}

/// The deprecated names each module's items reach: the root's, then `read`'s.
fn deprecated_alias_names(
    contracts: &Contracts,
    api: &ApiModel,
    api_modules: bool,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut root = BTreeSet::new();
    let mut read = BTreeSet::new();
    for contract in contracts.types() {
        let names = if contract.form().is_lenient() {
            &mut read
        } else {
            &mut root
        };
        for reference in contract.kind().references() {
            note_deprecated(reference, contracts, names);
        }
        // Its own impls name it: the hand-written serde ones, and the lenient decoder every
        // read-reachable type carries.
        if contract.docs().deprecated
            && (contract.deser() != DeserStrategy::Derive
                || matches!(contract.kind(), ContractKind::StringEnum { .. })
                || contract.form().is_read())
        {
            names.insert(contract.rust_name().as_str().to_owned());
        }
        // A twin's `From` impl names its strict half from inside `read`.
        if let Form::Lenient {
            strict: Some(strict),
        } = contract.form()
            && let Some(strict) = contracts.get(strict)
            && strict.docs().deprecated
        {
            root.insert(strict.rust_name().as_str().to_owned());
        }
    }
    if !api_modules {
        return (root, read);
    }
    for operation in api.operations() {
        for param in &operation.params {
            note_deprecated(&param.ty, contracts, &mut root);
        }
        if let Some(ty) = operation.body.as_ref().and_then(BodyContract::ty) {
            note_deprecated(ty, contracts, &mut root);
        }
        for arm in operation
            .responses
            .arms
            .iter()
            .chain(&operation.responses.default)
        {
            if let Some(ty) = arm.body.json_type() {
                // A response position names read forms, spelled through `read`'s aliases; a
                // shared type it names is re-exported there and aliased there too.
                let mut reached = Vec::new();
                ty.named(&mut reached);
                for index in reached {
                    if let Some(contract) = contracts.get(index)
                        && contract.docs().deprecated
                    {
                        let names = if contract.form().is_lenient() {
                            &mut read
                        } else {
                            &mut root
                        };
                        names.insert(contract.rust_name().as_str().to_owned());
                    }
                }
            }
        }
    }
    (root, read)
}

fn note_deprecated(ty: &TypeRef, contracts: &Contracts, names: &mut BTreeSet<String>) {
    let mut reached = Vec::new();
    ty.named(&mut reached);
    for index in reached {
        if let Some(contract) = contracts.get(index)
            && contract.docs().deprecated
        {
            names.insert(contract.rust_name().as_str().to_owned());
        }
    }
}

fn one(contract: &TypeContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let scope = Scope::of(contract);
    let name = ident(contract.rust_name());
    let docs = type_docs(contract);
    // The serde derives join the typed derive set in one attribute: which of them appears is the
    // serde strategy's business, and the strategy was decided by the eligibility function.
    let derives = derives(contract);

    match contract.kind() {
        ContractKind::Struct { fields } => {
            let body = struct_item(fields, contract, contracts, config, scope);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::Enum { variants, fallback } => {
            let body = data_enum(variants, fallback, contract, contracts, config, scope);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::TaggedEnum {
            tag,
            variants,
            fallback,
        } => {
            let body = tagged_enum(tag, variants, fallback, contract, contracts, config, scope);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::CarriedTagEnum {
            variants, fallback, ..
        } => {
            let body = carried_tag_enum(variants, fallback, contract, contracts, config, scope);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::StringEnum { variants, fallback } => {
            // Never a serde attribute here: an open string enum is hand-written under both
            // strategies, because no derive encoding keeps an unlisted string verbatim.
            let arms = variants.iter().map(|variant| {
                let variant_ident = ident(&variant.rust_name);
                quote! { #variant_ident, }
            });
            let fallback_ident = ident(fallback);
            let accessor = string_enum_accessor(contract, variants, fallback);
            quote! {
                #docs
                #derives
                pub enum #name {
                    #(#arms)*
                    /// A value the description does not list, kept exactly as it arrived.
                    ///
                    /// Written back unchanged, so a value round-trips whether or not the
                    /// description knows it, and a request can carry one the description has
                    /// not caught up with.
                    #fallback_ident(String),
                }
                #accessor
            }
        }
        ContractKind::Newtype { inner } => {
            let ty = type_ref_in(inner, contracts, config, scope);
            quote! {
                #docs
                #derives
                pub struct #name(pub #ty);
            }
        }
        ContractKind::Tuple { items } => {
            let members = items.iter().map(|item| {
                let ty = type_ref_in(item, contracts, config, scope);
                quote! { pub #ty, }
            });
            quote! {
                #docs
                #derives
                pub struct #name(#(#members)*);
            }
        }
        ContractKind::Alias { target } => {
            let ty = type_ref_in(target, contracts, config, scope);
            quote! {
                #docs
                pub type #name = #ty;
            }
        }
    }
}

/// The `as_str` accessor of a string enum: each variant's wire value.
///
/// The mapping already exists inside the type's `Serialize`, but reaching it there costs a
/// serializer round trip and a caller-side helper that accepts anything `Serialize` — the shape of
/// workaround this method exists to make unnecessary. A Rust variant name is progeny's spelling
/// and the wire value is the document's; this hands back the second one, which is the only one
/// that belongs in a payload or a lookup a caller assembles. Borrowed from `self` rather than
/// `'static`, because the fallback variant's string lives in the value.
fn string_enum_accessor(
    contract: &TypeContract,
    variants: &[crate::contract::StringVariant],
    fallback: &RustIdent,
) -> TokenStream {
    let name = super::serde_impl::implementation_type(contract);
    let arms = variants.iter().map(|variant| {
        let member = ident(&variant.rust_name);
        let wire = variant.wire_name.as_str();
        if contract.docs().deprecated {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the generated accessor must match this deprecated variant"
                )]
                Self::#member => #wire,
            }
        } else {
            quote! { Self::#member => #wire, }
        }
    });
    let fallback = ident(fallback);
    let unlisted = if contract.docs().deprecated {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated accessor must match this deprecated variant"
            )]
            Self::#fallback(value) => value,
        }
    } else {
        quote! { Self::#fallback(value) => value, }
    };
    quote! {
        impl #name {
            /// The string this value puts on the wire: the `enum` value the document declares,
            /// not the Rust variant name — or the unlisted value itself.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    #(#arms)*
                    #unlisted
                }
            }
        }
    }
}

/// An untagged data-carrying enum: matched by shape, so its variant names never touch the wire and
/// there is nothing to rename.
fn data_enum(
    variants: &[crate::contract::VariantContract],
    fallback: &RustIdent,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let tagging = match contract.deser() {
        DeserStrategy::Derive => quote! { #[serde(untagged)] },
        // The hand-written path reads the same contract and consults no attributes, so leaving one
        // on would be a second source of truth — and would not resolve without the derive anyway.
        DeserStrategy::HandWrittenBuffered { .. }
        | DeserStrategy::HandWrittenFieldless
        | DeserStrategy::HandWrittenCarriedTag => {
            quote! {}
        }
    };
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = enum_type_ref(&variant.ty, contracts, config, scope);
        quote! { #variant_ident(#ty), }
    });
    // Last, because an untagged enum takes the first variant that reads the payload and
    // arbitrary JSON reads every payload.
    let fallback = fallback_variant(fallback, "fits no variant's shape");
    quote! {
        #tagging
        pub enum #name {
            #(#arms)*
            #fallback
        }
    }
}

/// The open arm of a union: the payload as arbitrary JSON, boxed like every other variant.
fn fallback_variant(fallback: &RustIdent, when: &str) -> TokenStream {
    let fallback = ident(fallback);
    let summary = format!(" A payload that {when}, kept exactly as it arrived.");
    quote! {
        #[doc = #summary]
        #[doc = ""]
        #[doc = " Written back unchanged, so a value round-trips whether or not the description knows its shape."]
        #fallback(Box<serde_json::Value>),
    }
}

/// A carried-tag data-carrying enum: a plain enum item.
///
/// Nothing serde appears here on purpose. The tag member lives inside each variant's own type,
/// and both impls are always hand-written (`serde_impl.rs`), so the item is variants and nothing
/// else — the tag and the wire names are the impls' business.
fn carried_tag_enum(
    variants: &[crate::contract::TaggedVariant],
    fallback: &RustIdent,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = enum_type_ref(&variant.ty, contracts, config, scope);
        quote! { #variant_ident(#ty), }
    });
    let fallback = fallback_variant(fallback, "names no variant in its tag");
    quote! {
        pub enum #name {
            #(#arms)*
            #fallback
        }
    }
}

/// A tagged data-carrying enum: the tag member and each variant's exact wire name, straight off
/// the contract.
///
/// The tag attribute and the per-variant rename are two readings of one contract kind, which is
/// why they are written together: a tagged union has exactly one name per variant and no choice
/// about using it.
fn tagged_enum(
    tag: &str,
    variants: &[crate::contract::TaggedVariant],
    fallback: &RustIdent,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let tagging = match contract.deser() {
        DeserStrategy::Derive => quote! { #[serde(tag = #tag)] },
        // The hand-written path reads the same contract and consults no attributes, so leaving one
        // on would be a second source of truth — and would not resolve without the derive anyway.
        DeserStrategy::HandWrittenBuffered { .. }
        | DeserStrategy::HandWrittenFieldless
        | DeserStrategy::HandWrittenCarriedTag => {
            quote! {}
        }
    };
    let with_serde = contract.deser() == DeserStrategy::Derive;
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = enum_type_ref(&variant.ty, contracts, config, scope);
        let wire = &variant.tag_value;
        let rename =
            (with_serde && variant_ident != *wire).then(|| quote! { #[serde(rename = #wire)] });
        quote! { #rename #variant_ident(#ty), }
    });
    // serde tries every tagged variant first and falls through to an untagged one only when
    // none read the payload, which is exactly the open arm: a tag the description does not
    // list, or a payload the named variant cannot read.
    let untagged = with_serde.then(|| quote! { #[serde(untagged)] });
    let fallback = fallback_variant(fallback, "names no variant in its tag");
    quote! {
        #tagging
        pub enum #name {
            #(#arms)*
            #untagged
            #fallback
        }
    }
}

/// An expectation for a field whose type exceeds Clippy's default complexity threshold.
///
/// Kept on the field rather than its struct so an unrelated field cannot satisfy it. The score
/// mirrors Clippy's type visitor: paths, slices, tuples, and arrays cost ten times their nesting
/// depth; references and pointers cost one. Generated types do not contain bare function or trait
/// object types, but those cases are included so this stays correct if the renderer grows them.
pub(super) fn type_complexity(ty: &TokenStream) -> TokenStream {
    use syn::visit::Visit as _;

    let Ok(ty) = syn::parse2::<syn::Type>(ty.clone()) else {
        return TokenStream::new();
    };
    let mut visitor = TypeComplexity { score: 0, nest: 1 };
    visitor.visit_type(&ty);
    if visitor.score <= 250 {
        return TokenStream::new();
    }
    quote! {
        #[expect(
            clippy::type_complexity,
            reason = "the public field type mirrors the schema and must remain explicit"
        )]
    }
}

struct TypeComplexity {
    score: u64,
    nest: u64,
}

impl<'ast> syn::visit::Visit<'ast> for TypeComplexity {
    fn visit_type(&mut self, ty: &'ast syn::Type) {
        let (score, nesting) = match ty {
            syn::Type::Ptr(_) | syn::Type::Reference(_) => (1, 0),
            syn::Type::Path(_)
            | syn::Type::Slice(_)
            | syn::Type::Tuple(_)
            | syn::Type::Array(_) => (10 * self.nest, 1),
            syn::Type::FnPtr(_) => (50 * self.nest, 1),
            syn::Type::TraitObject(_) => (20 * self.nest, 0),
            _ => (0, 0),
        };
        self.score += score;
        self.nest += nesting;
        syn::visit::visit_type(self, ty);
        self.nest -= nesting;
    }
}

/// A type's doc comment.
///
/// A type the document documented says what it is; one it did not says where it came from, which
/// is the next most useful thing for someone reading checked-in generated source. A read form
/// says so first, because its members are not the description's members.
fn type_docs(contract: &TypeContract) -> TokenStream {
    let described = if contract.docs().is_empty() {
        let origin = format!(" Generated from `{}`.", contract.origin());
        quote! { #[doc = #origin] }
    } else {
        docs(contract.docs())
    };
    // What the read form changed depends on the kind: a struct loosened its members and grew a
    // capture map, everything else only names the read forms of what it holds.
    let shape = if matches!(contract.kind(), ContractKind::Struct { .. }) {
        quote! {
            /// The read form: what a response decodes into. Every member is optional and
            /// undeclared members are kept in `extra`.
        }
    } else {
        quote! {
            /// The read form: what a response decodes into, naming the read forms of what it
            /// holds.
        }
    };
    match contract.form() {
        Form::Lenient { strict: Some(_) } => quote! {
            #shape
            /// The strict form in the parent module is what a request sends, and converts into
            /// this with `From`.
            ///
            #described
        },
        Form::Lenient { strict: None } => quote! {
            #shape
            ///
            #described
        },
        Form::Strict { .. } | Form::Shared => described,
    }
}

/// A struct's declaration: its serde attribute, if any, and one member per field.
fn struct_item(
    fields: &[FieldContract],
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = ident(contract.rust_name());
    // A read form has no derived `Deserialize` to deny anything on.
    let deny = match (contract.deser(), contract.unknown_fields()) {
        (DeserStrategy::Derive, crate::config::UnknownFields::Deny)
            if !contract.form().is_lenient() =>
        {
            quote! { #[serde(deny_unknown_fields)] }
        }
        _ => quote! {},
    };
    let members = fields
        .iter()
        .map(|field| member(field, contract, contracts, config, scope));
    quote! {
        #deny
        pub struct #name {
            #(#members)*
        }
    }
}

fn member(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = ident(&field.rust_name);
    let ty = type_ref_in(&field.ty, contracts, config, scope);
    let nesting = type_complexity(&ty);
    let docs = with_default(docs(&field.docs), field);
    if contract.deser() != DeserStrategy::Derive {
        // No serde attributes at all: the hand-written implementation reads the same contract and
        // does not consult attributes, so leaving them on would be a second source of truth.
        return quote! { #docs #nesting pub #name: #ty, };
    }
    let support = scope.support();

    let mut attributes = Vec::new();
    if field.is_capture() {
        attributes.push(quote! { flatten });
    } else if name != field.wire_name {
        let wire = &field.wire_name;
        attributes.push(quote! { rename = #wire });
    }
    if field.skip_serializing_if == SkipRule::WhenNone && !field.is_capture() {
        attributes.push(quote! { skip_serializing_if = "Option::is_none" });
    }
    if field.skip_serializing_if == SkipRule::WhenOmitted && !field.is_capture() {
        let is_omitted = format!("{support}::Presence::is_omitted");
        attributes.push(quote! {
            default,
            skip_serializing_if = #is_omitted
        });
    }
    let serde = (!attributes.is_empty()).then(|| quote! { #[serde(#(#attributes),*)] });
    quote! { #docs #serde #nesting pub #name: #ty, }
}

/// A field's declared default, said in its documentation rather than applied on deserialize.
///
/// **`#[serde(default = "…")]` would be a wire defect.** OpenAPI's `default` states what the *server*
/// assumes when a member is absent; serde's fills the field in on the way *in*, and the field is
/// then written on the way *out*. On a request body that turns "the caller said nothing" into "the
/// caller said `false`" — a different request, sent silently, which is the one forbidden failure
/// mode. The payload gate caught it on its first run over the corpus: 60 examples across three
/// documents came back carrying members they never had.
///
/// Nothing is lost by dropping the attribute, because every non-required field is an `Option` and
/// serde reads an absent `Option` as `None` without being told to. What *is* lost is the convenience
/// of reading a server's default off an absent member, so the value is said out loud instead.
fn with_default(docs: TokenStream, field: &FieldContract) -> TokenStream {
    let Some(default) = &field.default else {
        return docs;
    };
    let note = format!(" The server assumes `{default}` when this member is absent.");
    quote! { #docs #[doc = ""] #[doc = #note] }
}

fn derives(contract: &TypeContract) -> TokenStream {
    if matches!(contract.kind(), ContractKind::Alias { .. }) {
        return quote! {};
    }
    let names = contract
        .derives()
        .iter()
        .map(|derive| format_ident!("{}", derive.name()));
    // The serde derives are only present under the derive strategy; the hand-written path carries
    // no serde attributes at all, so it must carry no serde derive either. A read form's
    // `Deserialize` goes through the lenient decoder and is written by hand under both.
    let serde = match (contract.deser(), contract.form().is_lenient()) {
        (DeserStrategy::Derive, false) => quote! { , serde::Serialize, serde::Deserialize },
        (DeserStrategy::Derive, true) => quote! { , serde::Serialize },
        (
            DeserStrategy::HandWrittenBuffered { .. }
            | DeserStrategy::HandWrittenFieldless
            | DeserStrategy::HandWrittenCarriedTag,
            _,
        ) => {
            quote! {}
        }
    };
    quote! { #[derive(#(#names),* #serde)] }
}

/// A type reference, spelled out from the given scope.
pub(crate) fn type_ref_in(
    ty: &TypeRef,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    reference(ty, contracts, config, scope)
}

/// The same type, named from outside the `types` module.
///
/// A named type renders as a bare identifier inside `types.rs` and must not anywhere else: the
/// client module re-exports `Error` from the support module, and a document with a schema called
/// `Error` — the petstore has one — would otherwise produce `Error<Error>` whose two `Error`s are
/// different types. The bug is silent, because it still compiles.
pub(crate) fn type_path(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    reference(ty, contracts, config, Scope::Edge)
}

/// A response payload named from outside the `types` module.
pub(crate) fn response_type_path(
    body: &ResponseBody,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    match body {
        ResponseBody::Json { ty, .. } => type_path(ty, contracts, config),
        ResponseBody::Text { .. } => quote! { ::std::string::String },
        ResponseBody::Bytes { .. } => bytes_type(config),
        ResponseBody::Empty => quote! { () },
    }
}

/// An enum's response payload named from outside the `types` module.
///
/// Every non-empty payload is indirected uniformly rather than according to dependency-defined
/// layouts. That bounds every generated enum without making its API change when a configured
/// representation changes size.
pub(crate) fn response_enum_type_path(
    body: &ResponseBody,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let rendered = response_type_path(body, contracts, config);
    if response_body_is_boxed(body) {
        quote! { Box<#rendered> }
    } else {
        rendered
    }
}

/// Whether a response enum payload receives the stable non-unit indirection.
pub(crate) fn response_body_is_boxed(body: &ResponseBody) -> bool {
    !matches!(body, ResponseBody::Empty)
}

/// Whether an enum payload receives the stable non-unit indirection.
pub(crate) fn enum_type_is_boxed(ty: &TypeRef) -> bool {
    !matches!(ty, TypeRef::Unit)
}

fn enum_type_ref(
    ty: &TypeRef,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let rendered = reference(ty, contracts, config, scope);
    if enum_type_is_boxed(ty) {
        quote! { Box<#rendered> }
    } else {
        rendered
    }
}

fn reference(ty: &TypeRef, contracts: &Contracts, config: &Config, scope: Scope) -> TokenStream {
    let type_ref = |inner: &TypeRef| reference(inner, contracts, config, scope);
    let support = scope.support();
    match ty {
        TypeRef::Named(index) => {
            if let Some(contract) = contracts.get(*index) {
                let name = ident(contract.rust_name());
                // A read form lives in `read`; everything else in the root. A shared type is
                // re-exported into `read`, so from inside it every type a response can yield is
                // a bare name; a strict type is not — its twin holds that name there — and is
                // reached in the root, which only a twin's conversion from it ever does.
                let module = match (scope, contract.form()) {
                    (Scope::Read, Form::Lenient { .. } | Form::Shared)
                    | (Scope::Types, Form::Strict { .. } | Form::Shared) => quote! {},
                    (Scope::Read, Form::Strict { .. }) => quote! { super:: },
                    (Scope::Types, Form::Lenient { .. }) => quote! { read:: },
                    (Scope::Edge, Form::Strict { .. } | Form::Shared) => quote! { super::types:: },
                    (Scope::Edge, Form::Lenient { .. }) => quote! { super::types::read:: },
                };
                if contract.docs().deprecated {
                    quote! { #module __progeny_deprecated::#name }
                } else {
                    quote! { #module #name }
                }
            } else {
                // Unreachable: every index comes from the contract set it is rendered against.
                quote! { serde_json::Value }
            }
        }
        TypeRef::Unit => quote! { () },
        TypeRef::Bool => quote! { bool },
        TypeRef::I64 => quote! { i64 },
        TypeRef::U64 => quote! { u64 },
        TypeRef::F64 => quote! { f64 },
        TypeRef::String => quote! { String },
        TypeRef::Format(format) => format_type(*format, config),
        // The types module and the support module are siblings in every packaging, whether
        // they live in one crate or the types crate carries both; `read` is one level further in.
        TypeRef::Upload => quote! { #support::Upload },
        TypeRef::Value => quote! { serde_json::Value },
        TypeRef::Option(inner) => {
            let inner = type_ref(inner);
            quote! { Option<#inner> }
        }
        TypeRef::Presence(inner) => {
            let inner = type_ref(inner);
            quote! { #support::Presence<#inner> }
        }
        TypeRef::Vec(inner) => {
            let inner = type_ref(inner);
            quote! { Vec<#inner> }
        }
        TypeRef::Map(inner) => {
            let inner = type_ref(inner);
            match config.map {
                MapKind::BTreeMap => quote! { std::collections::BTreeMap<String, #inner> },
                MapKind::HashMap => quote! { std::collections::HashMap<String, #inner> },
                MapKind::IndexMap => quote! { indexmap::IndexMap<String, #inner> },
            }
        }
        TypeRef::Array(inner, len) => {
            let inner = type_ref(inner);
            let len = usize::try_from(*len).unwrap_or(0);
            quote! { [#inner; #len] }
        }
        TypeRef::Tuple(items) => {
            let items = items.iter().map(&type_ref);
            quote! { (#(#items),*) }
        }
        TypeRef::Boxed(inner) => {
            let inner = type_ref(inner);
            quote! { Box<#inner> }
        }
    }
}

/// The type a format renders as, which is the caller's choice.
///
/// `Base64` and `Binary` are `String` whatever the byte representation is set to, and that is not an
/// omission: inside a JSON payload a base64 or binary property *is* a string, and turning it into
/// bytes needs a codec the generated crate does not have. The byte representation applies to a raw
/// binary request or response body, which is the API model's business.
fn format_type(format: Format, config: &Config) -> TokenStream {
    match format {
        Format::DateTime => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::DateTime<chrono::Utc> },
            DateTimeCrate::Time => quote! { time::OffsetDateTime },
            DateTimeCrate::Jiff => quote! { jiff::Timestamp },
        },
        Format::Date => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::NaiveDate },
            DateTimeCrate::Time => quote! { time::Date },
            DateTimeCrate::Jiff => quote! { jiff::civil::Date },
        },
        Format::Time => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::NaiveTime },
            DateTimeCrate::Time => quote! { time::Time },
            DateTimeCrate::Jiff => quote! { jiff::civil::Time },
        },
        Format::Uuid => match config.formats.uuid {
            UuidCrate::String => quote! { String },
            UuidCrate::Uuid => quote! { uuid::Uuid },
        },
        Format::Ip => quote! { ::std::net::IpAddr },
        Format::Ipv4 => quote! { ::std::net::Ipv4Addr },
        Format::Ipv6 => quote! { ::std::net::Ipv6Addr },
        Format::Base64 | Format::Binary => quote! { String },
    }
}

fn bytes_type(config: &Config) -> TokenStream {
    match config.formats.bytes {
        BytesRepr::Vec => quote! { ::std::vec::Vec<u8> },
        BytesRepr::Bytes => quote! { ::bytes::Bytes },
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}

/// Doc comments, one attribute per line so a multi-line description reads as one.
pub(super) fn docs(docs: &Docs) -> TokenStream {
    let prose = docs_prose(docs);
    let deprecated = docs.deprecated.then(|| quote! { #[deprecated] });
    quote! { #prose #deprecated }
}

/// The doc comments alone, without the `#[deprecated]` attribute.
///
/// For a position a consumer cannot avoid naming: a params-struct field appears in every literal
/// the compiler accepts, so the attribute would make the deprecation warning unavoidable at
/// every call site. The fact rides the field's prose instead, and the operation-level attribute
/// still marks the API a caller can choose not to call.
pub(super) fn docs_prose(docs: &Docs) -> TokenStream {
    let mut lines: Vec<String> = Vec::new();
    if let Some(title) = &docs.title {
        lines.extend(wrap(title));
    }
    if let Some(description) = &docs.description {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(wrap(description));
    }
    let attributes = lines.iter().map(|line| {
        let text = format!(" {line}");
        quote! { #[doc = #text] }
    });
    quote! { #(#attributes)* }
}

/// Split a description into doc-comment lines, in markdown a consumer's build will not complain
/// about.
///
/// Vendor prose is transcribed, never rewritten — but the transcription has to survive being read
/// as rustdoc markdown, and three things in real descriptions do not. A **tab**, whose width
/// rustdoc does not define. And the two forms of **lazy continuation**: a paragraph line belonging
/// to a list item or a blockquote that leaves out the indent, or the `>`, that would say so
/// explicitly. `CommonMark` defines each as equivalent to its explicit form, so writing the explicit
/// form renders identically and removes a warning the consumer would otherwise get in their own
/// build, about prose they did not write.
///
/// This is not hypothetical markdown pedantry. `posthog` describes an endpoint with a parenthesis
/// that wraps onto a line starting `+ the spec it derived…`, which markdown reads as a list item
/// and every line after it as a lazy continuation of one — 79 warnings from one habit of writing.
///
/// Fenced code keeps its indentation, because inside a fence indentation is content. Its tabs are
/// still expanded — the lint fires there too, and four-column stops are what rustdoc would have
/// shown anyway.
fn wrap(text: &str) -> Vec<String> {
    let expanded: Vec<String> = text.replace('\r', "").lines().map(expand_tabs).collect();
    // Rustdoc removes the indentation every line of a doc comment shares before reading it as
    // markdown, so every column measured below has to be measured after the same removal or the
    // two disagree about what the document says. `sentry` writes a description indented twelve
    // columns throughout: read literally that is one long indented code block, read as rustdoc
    // reads it those are list items at column zero with lazy continuations under them — and it is
    // rustdoc that emits the warning.
    let common = expanded
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    // What a lazy line in the block now open should have been written with, and whether a
    // paragraph is open inside it. Two pieces of state rather than one, because a blank line ends
    // the paragraph *without* closing the block: a list item continues across one, and `okta`
    // writes a second paragraph inside an item and then wraps it lazily back to column zero.
    // Collapsing them loses the item at the blank line and leaves everything after it unindented.
    let mut continuation: Option<String> = None;
    let mut paragraph = false;
    for raw in &expanded {
        let line = raw.get(common..).unwrap_or_default().to_owned();
        if let Some(open) = &fence {
            if closes_fence(&line, open) {
                fence = None;
            }
            out.push(line);
            continue;
        }
        if let Some(open) = opens_fence(&line) {
            fence = Some(open);
            paragraph = false;
            out.push(line);
            continue;
        }
        if line.trim().is_empty() {
            paragraph = false;
            out.push(line);
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        // Where the innermost open block's content starts. Every indentation question below is
        // asked relative to this rather than to column zero, which is the whole difficulty with
        // nested lists: `orb` writes a sub-item at column 4, and read absolutely that is an
        // indented code block, while read against its parent's content column of 2 it is what it
        // looks like. Getting that backwards flattens the sub-item and strands its continuation.
        let content = continuation.as_ref().map_or(0, String::len);
        // An indented code block **cannot interrupt a paragraph** — `CommonMark` says so, and it is
        // the difference between a code block and an over-indented continuation. `langsmith` wraps
        // a list item's prose onto a line indented twelve columns under an item whose content
        // starts at two; passing it through as code leaves rustdoc reading it as a list item at the
        // wrong column, which is the warning it then emits.
        if !paragraph && indent >= content + 4 {
            out.push(line);
            continue;
        }
        if let Some(prefix) = block_prefix(&line, indent) {
            continuation = Some(prefix);
            paragraph = true;
            out.push(line);
            continue;
        }
        if paragraph && let Some(prefix) = &continuation {
            out.push(continued(&line, prefix));
            continue;
        }
        // The first line of a paragraph after a blank one. It stays inside the open block when it
        // is indented into it, and closes the block when it is not.
        if indent < content {
            continuation = None;
        }
        paragraph = true;
        out.push(line);
    }
    out
}

/// Tabs, as the four-column stops rustdoc assumes and does not promise.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len());
    for character in line.chars() {
        if character == '\t' {
            let width = 4 - (out.chars().count() % 4);
            out.extend(std::iter::repeat_n(' ', width));
        } else {
            out.push(character);
        }
    }
    out
}

fn opens_fence(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    for marker in ['`', '~'] {
        let run = trimmed.chars().take_while(|it| *it == marker).count();
        if run >= 3 {
            return Some(std::iter::repeat_n(marker, run).collect());
        }
    }
    None
}

fn closes_fence(line: &str, open: &str) -> bool {
    let trimmed = line.trim();
    let marker = open.chars().next().unwrap_or('`');
    trimmed.len() >= open.len() && trimmed.chars().all(|it| it == marker)
}

/// What continuations of the block this line opens have to be written with, if it opens one.
///
/// The caller has already ruled out an indented code block, relative to the block now open.
fn block_prefix(line: &str, indent: usize) -> Option<String> {
    let rest = &line[indent..];
    if rest.starts_with('>') {
        return Some(format!("{}> ", " ".repeat(indent)));
    }
    let marker = list_marker(rest)?;
    // A list item's continuations line up with its content, which is where clippy points.
    Some(" ".repeat(indent + marker))
}

/// The width of the list marker this line starts with, including the space after it.
fn list_marker(rest: &str) -> Option<usize> {
    let mut chars = rest.chars();
    let first = chars.next()?;
    let width = if matches!(first, '-' | '*' | '+') {
        1
    } else if first.is_ascii_digit() {
        // `1.` and `1)` both start an ordered list; `1` alone starts a sentence.
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if !matches!(rest.chars().nth(digits), Some('.' | ')')) {
            return None;
        }
        digits + 1
    } else {
        return None;
    };
    // Without a space it is emphasis, a horizontal rule, or a number — not a list.
    let after = &rest.get(width..)?;
    let spaces = after.len() - after.trim_start_matches(' ').len();
    (spaces > 0).then_some(width + spaces)
}

/// A lazy line, written out with the prefix it left implicit.
fn continued(line: &str, prefix: &str) -> String {
    format!("{prefix}{}", line.trim_start_matches(' '))
}

#[cfg(test)]
mod doc_tests {
    use super::wrap;

    fn normalized(text: &str) -> String {
        wrap(text).join("\n")
    }

    #[test]
    fn a_lazy_list_continuation_is_written_out() {
        // `posthog`: a parenthesis wraps onto a line starting `+ `, which markdown reads as a list
        // item — and every line after it as a lazy continuation of one.
        let found = normalized(indoc::indoc! {"
            ask the janitor to seal it (the janitor returns the sha
            + the spec it derived), then stamp the
            row. No rollback."
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                ask the janitor to seal it (the janitor returns the sha
                + the spec it derived), then stamp the
                  row. No rollback."
            }
        );
    }

    #[test]
    fn an_overindented_list_continuation_is_pulled_back_to_its_content() {
        // The same rule from the other side: the continuation belongs at the item's content
        // column, whether the vendor wrote too little indentation or too much.
        let found = normalized(indoc::indoc! {"
            * If part index is included: the file matching the index (as ordered
                by key) is downloaded."
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                * If part index is included: the file matching the index (as ordered
                  by key) is downloaded."
            }
        );
    }

    #[test]
    fn a_lazy_quote_continuation_gets_its_marker() {
        // `okta` writes deprecation notices as blockquotes whose second line drops the `>`.
        let found = normalized(indoc::indoc! {"
            > **Note:** This property isn't supported.
            See the deprecation notice."
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                > **Note:** This property isn't supported.
                > See the deprecation notice."
            }
        );
    }

    #[test]
    fn a_blank_line_ends_the_block_rather_than_capturing_what_follows() {
        // Lazy continuation is a within-paragraph rule. Indenting past a blank line would move a
        // new paragraph *into* the list, which changes what the document says.
        let found = normalized(indoc::indoc! {"
            * an item

            A new paragraph."
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                * an item

                A new paragraph."
            }
        );
    }

    #[test]
    fn fenced_code_is_left_exactly_as_written() {
        // Inside a fence, indentation is content.
        let found = normalized(indoc::indoc! {"
            * an item
            ```
            not   a continuation
                indented on purpose
            ```
            tail"
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                * an item
                ```
                not   a continuation
                    indented on purpose
                ```
                tail"
            }
        );
    }

    #[test]
    fn tabs_become_spaces_because_rustdoc_does_not_define_their_width() {
        assert_eq!(normalized("a\tb"), "a   b");
        // Expanded before anything measures a column, so a tab-indented line is four columns in
        // relative to its neighbours — and four columns in from a neighbour at zero, not from
        // nothing, which is why this needs a second line to be a test of indentation at all.
        assert_eq!(
            normalized(indoc::indoc! {"
                prose

                \tindented"
            }),
            indoc::indoc! {"
                prose

                    indented"
            }
        );
    }

    #[test]
    fn what_only_looks_like_a_list_is_left_alone() {
        // Emphasis, a horizontal rule and a sentence that opens with a number all start with a
        // list marker's first character and none of them is a list.
        assert_eq!(
            normalized(indoc::indoc! {"
                *emphasis*
                continues"
            }),
            indoc::indoc! {"
                *emphasis*
                continues"
            }
        );
        assert_eq!(
            normalized(indoc::indoc! {"
                ---
                continues"
            }),
            indoc::indoc! {"
                ---
                continues"
            }
        );
        assert_eq!(
            normalized(indoc::indoc! {"
                2024 was the year
                it changed"
            }),
            indoc::indoc! {"
                2024 was the year
                it changed"
            }
        );
    }

    #[test]
    fn an_indented_code_block_under_a_list_item_keeps_its_indentation() {
        // Four past the content column *and* after a blank line, which is what makes it code.
        // Without the blank line it is a continuation of the item's paragraph, because an indented
        // code block cannot interrupt one — see `paragraph_doc_tests`, and `langsmith`, where
        // rustdoc says so out loud.
        let found = normalized(indoc::indoc! {"
            * an item

                  code, four past the content column"
        });
        assert_eq!(
            found,
            indoc::indoc! {"
                * an item

                      code, four past the content column"
            }
        );
    }
}

#[cfg(test)]
mod nested_doc_tests {
    use super::wrap;

    #[test]
    fn a_sub_item_is_read_against_its_parent_rather_than_column_zero() {
        // `orb` writes a sub-item at column 4 under an item whose content starts at column 2.
        // Read absolutely that is an indented code block; read against its parent it is a list.
        // The first reading flattens the sub-item and strands its own continuation above it.
        let found = wrap(indoc::indoc! {"
            - outer item wrapping
              its continuation:
                - inner item wrapping
                  its continuation."
        })
        .join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                - outer item wrapping
                  its continuation:
                    - inner item wrapping
                      its continuation."
            }
        );
    }

    #[test]
    fn a_lazy_line_under_a_sub_item_lines_up_with_the_sub_item() {
        let found = wrap(indoc::indoc! {"
            - outer
                - inner item wrapping
            its lazy continuation."
        })
        .join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                - outer
                    - inner item wrapping
                      its lazy continuation."
            }
        );
    }
}

#[cfg(test)]
mod unindent_doc_tests {
    use super::wrap;

    #[test]
    fn a_description_indented_throughout_is_read_the_way_rustdoc_reads_it() {
        // `sentry` writes descriptions indented twelve columns from end to end. Measured against
        // column zero every line is an indented code block; rustdoc removes the shared indentation
        // first and sees list items with a lazy continuation, and rustdoc is the one that warns.
        let input = indoc::formatdoc! {"
            {indent}- `comparisonDelta`: the comparison delta, in minutes.
            {indent}For example, 3600 compares against data one hour ago.",
            indent = "            "
        };
        let found = wrap(&input).join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                - `comparisonDelta`: the comparison delta, in minutes.
                  For example, 3600 compares against data one hour ago."
            }
        );
    }

    #[test]
    fn removing_the_shared_indent_keeps_every_relative_indent() {
        // Only what *every* line shares comes off, so a nested item stays nested and a genuine
        // code block stays a code block.
        let input = indoc::formatdoc! {"
            {indent}Prose.

            {indent}- an item

            {indent}    code under it

            {indent}Back to prose.",
            indent = "  "
        };
        let found = wrap(&input).join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                Prose.

                - an item

                    code under it

                Back to prose."
            }
        );
    }

    #[test]
    fn a_blank_line_shorter_than_the_shared_indent_survives() {
        // A truly empty line has no indentation to contribute and must not be counted, or the
        // shared indent is always zero and nothing is ever unindented.
        let input = indoc::formatdoc! {"
            {indent}first

            {indent}second",
            indent = "    "
        };
        let found = wrap(&input).join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                first

                second"
            }
        );
    }
}

#[cfg(test)]
mod paragraph_doc_tests {
    use super::wrap;

    #[test]
    fn a_second_paragraph_inside_a_list_item_keeps_the_item_open() {
        // `okta` writes a list item, a blank line, a second paragraph still inside the item, and
        // then wraps that paragraph lazily back to column zero. Treating the blank line as closing
        // the *item* rather than the paragraph loses the indent for everything after it.
        let found = wrap(indoc::indoc! {"
              * An optional filter. This is a rule.
                See the guide.

                Additionally, you can specify a key
            you must supply when calling.
            Each call."
        })
        .join("\n");
        let expected = indoc::formatdoc! {"
            {indent}* An optional filter. This is a rule.
            {indent}  See the guide.

            {indent}  Additionally, you can specify a key
            {indent}  you must supply when calling.
            {indent}  Each call.",
            indent = "  "
        };
        assert_eq!(found, expected);
    }

    #[test]
    fn an_indented_code_block_cannot_interrupt_a_paragraph() {
        // `langsmith` wraps a list item's prose onto a line indented twelve columns, under an item
        // whose content starts at two. Read as a code block it is passed through and rustdoc then
        // reads it as a list item at the wrong column; `CommonMark` says an indented code block
        // cannot interrupt a paragraph, so it is a continuation and belongs at the item's content.
        let found = wrap(indoc::indoc! {"
            - examples: shared examples across all sessions
                        with flat array of runs"
        })
        .join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                - examples: shared examples across all sessions
                  with flat array of runs"
            }
        );
    }

    #[test]
    fn a_code_block_after_a_blank_line_is_still_a_code_block() {
        // The other side of the same rule, so the fix cannot quietly reflow real code.
        let found = wrap(indoc::indoc! {"
            Some prose.

                fn main() {}
                // still code"
        })
        .join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                Some prose.

                    fn main() {}
                    // still code"
            }
        );
    }

    #[test]
    fn a_paragraph_that_leaves_the_item_closes_it() {
        // The other half of the same rule: after the blank line, a line at column zero is a new
        // paragraph outside the list, and indenting it into the item would change what it says.
        let found = wrap(indoc::indoc! {"
              * an item

            Back to the body text.
            Still the body text."
        })
        .join("\n");
        assert_eq!(
            found,
            indoc::indoc! {"
                  * an item

                Back to the body text.
                Still the body text."
            }
        );
    }
}
