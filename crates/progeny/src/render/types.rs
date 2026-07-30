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
    ContractKind, Contracts, DeserStrategy, FieldContract, RustIdent, SkipRule, TypeContract,
    TypeRef,
};
use crate::shape::{Docs, Format};

/// Render every type in the contract set.
pub(super) fn render(
    contracts: &Contracts,
    api: &ApiModel,
    api_modules: bool,
    external_api_modules: bool,
    config: &Config,
) -> TokenStream {
    let deprecated = deprecated_aliases(contracts, api, api_modules, external_api_modules);
    let items = contracts
        .types()
        .iter()
        .map(|contract| one(contract, contracts, config));
    quote! { #deprecated #(#items)* }
}

/// Non-deprecated paths the generated implementation uses to name deprecated public contracts.
///
/// The public declaration keeps its `#[deprecated]` marker, so callers still get the intended
/// warning. Generated derives and support code use these transparent aliases instead: procedural
/// macro expansions cannot inherit a field's `#[expect]`, so aliases are the only way to keep those
/// internal uses clean without a broad `#[allow(deprecated)]`.
fn deprecated_aliases(
    contracts: &Contracts,
    api: &ApiModel,
    api_modules: bool,
    external_api_modules: bool,
) -> TokenStream {
    let needed = deprecated_alias_names(contracts, api, api_modules);
    let visibility = if external_api_modules {
        quote! { pub }
    } else {
        quote! { pub(crate) }
    };
    let aliases: Vec<TokenStream> = contracts
        .types()
        .iter()
        .filter(|contract| {
            contract.docs().deprecated && needed.contains(contract.rust_name().as_str())
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

fn deprecated_alias_names(
    contracts: &Contracts,
    api: &ApiModel,
    api_modules: bool,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for contract in contracts.types() {
        for reference in contract.kind().references() {
            note_deprecated(reference, contracts, &mut names);
        }
        if contract.docs().deprecated && contract.deser() != DeserStrategy::Derive {
            names.insert(contract.rust_name().as_str().to_owned());
        }
    }
    if !api_modules {
        return names;
    }
    for operation in api.operations() {
        for param in &operation.params {
            note_deprecated(&param.ty, contracts, &mut names);
        }
        if let Some(ty) = operation.body.as_ref().and_then(BodyContract::ty) {
            note_deprecated(ty, contracts, &mut names);
        }
        for arm in operation
            .responses
            .arms
            .iter()
            .chain(&operation.responses.default)
        {
            if let Some(ty) = arm.body.json_type() {
                note_deprecated(ty, contracts, &mut names);
            }
        }
    }
    names
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
    let name = ident(contract.rust_name());
    // A type the document documented says what it is; one it did not says where it came from, which
    // is the next most useful thing for someone reading checked-in generated source.
    let docs = if contract.docs().is_empty() {
        let origin = format!(" Generated from `{}`.", contract.origin());
        quote! { #[doc = #origin] }
    } else {
        docs(contract.docs())
    };
    // The serde derives join the typed derive set in one attribute: which of them appears is the
    // serde strategy's business, and the strategy was decided by the eligibility function.
    let derives = derives(contract);

    match contract.kind() {
        ContractKind::Struct { fields } => {
            let deny = match (contract.deser(), contract.unknown_fields()) {
                (DeserStrategy::Derive, crate::config::UnknownFields::Deny) => {
                    quote! { #[serde(deny_unknown_fields)] }
                }
                _ => quote! {},
            };
            let members = fields
                .iter()
                .map(|field| member(field, contract, contracts, config));
            quote! {
                #docs
                #derives
                #deny
                pub struct #name {
                    #(#members)*
                }
            }
        }
        ContractKind::Enum { variants } => {
            let body = data_enum(variants, contract, contracts, config);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::TaggedEnum { tag, variants } => {
            let body = tagged_enum(tag, variants, contract, contracts, config);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::StringEnum { variants } => {
            let with_serde = contract.deser() == DeserStrategy::Derive;
            let arms = variants.iter().map(|variant| {
                let variant_ident = ident(&variant.rust_name);
                let wire = &variant.wire_name;
                // Only under the derive: with no serde derive on the item, a `#[serde(...)]` helper
                // attribute does not even resolve, and the crate fails to compile. Stripping them is
                // mandatory rather than tidy.
                let rename = (with_serde && variant_ident != *wire)
                    .then(|| quote! { #[serde(rename = #wire)] });
                quote! { #rename #variant_ident, }
            });
            quote! {
                #docs
                #derives
                pub enum #name {
                    #(#arms)*
                }
            }
        }
        ContractKind::Newtype { inner } => {
            let ty = type_ref(inner, contracts, config);
            quote! {
                #docs
                #derives
                pub struct #name(pub #ty);
            }
        }
        ContractKind::Tuple { items } => {
            let members = items.iter().map(|item| {
                let ty = type_ref(item, contracts, config);
                quote! { pub #ty, }
            });
            quote! {
                #docs
                #derives
                pub struct #name(#(#members)*);
            }
        }
        ContractKind::Alias { target } => {
            let ty = type_ref(target, contracts, config);
            quote! {
                #docs
                pub type #name = #ty;
            }
        }
    }
}

/// An untagged data-carrying enum: matched by shape, so its variant names never touch the wire and
/// there is nothing to rename.
fn data_enum(
    variants: &[crate::contract::VariantContract],
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let tagging = match contract.deser() {
        DeserStrategy::Derive => quote! { #[serde(untagged)] },
        // The hand-written path reads the same contract and consults no attributes, so leaving one
        // on would be a second source of truth — and would not resolve without the derive anyway.
        DeserStrategy::HandWrittenBuffered { .. } | DeserStrategy::HandWrittenFieldless => {
            quote! {}
        }
    };
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = enum_type_ref(&variant.ty, contracts, config);
        quote! { #variant_ident(#ty), }
    });
    quote! {
        #tagging
        pub enum #name {
            #(#arms)*
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
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let tagging = match contract.deser() {
        DeserStrategy::Derive => quote! { #[serde(tag = #tag)] },
        // The hand-written path reads the same contract and consults no attributes, so leaving one
        // on would be a second source of truth — and would not resolve without the derive anyway.
        DeserStrategy::HandWrittenBuffered { .. } | DeserStrategy::HandWrittenFieldless => {
            quote! {}
        }
    };
    let with_serde = contract.deser() == DeserStrategy::Derive;
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = enum_type_ref(&variant.ty, contracts, config);
        let wire = &variant.tag_value;
        let rename =
            (with_serde && variant_ident != *wire).then(|| quote! { #[serde(rename = #wire)] });
        quote! { #rename #variant_ident(#ty), }
    });
    quote! {
        #tagging
        pub enum #name {
            #(#arms)*
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
            syn::Type::BareFn(_) => (50 * self.nest, 1),
            syn::Type::TraitObject(_) => (20 * self.nest, 0),
            _ => (0, 0),
        };
        self.score += score;
        self.nest += nesting;
        syn::visit::visit_type(self, ty);
        self.nest -= nesting;
    }
}

fn member(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(&field.rust_name);
    let ty = type_ref(&field.ty, contracts, config);
    let nesting = type_complexity(&ty);
    let docs = with_default(docs(&field.docs), field);
    if contract.deser() != DeserStrategy::Derive {
        // No serde attributes at all: the hand-written implementation reads the same contract and
        // does not consult attributes, so leaving them on would be a second source of truth.
        return quote! { #docs #nesting pub #name: #ty, };
    }

    let mut attributes = Vec::new();
    if field.flatten {
        attributes.push(quote! { flatten });
    } else if name != field.wire_name {
        let wire = &field.wire_name;
        attributes.push(quote! { rename = #wire });
    }
    if field.skip_serializing_if == SkipRule::WhenNone && !field.flatten {
        attributes.push(quote! { skip_serializing_if = "Option::is_none" });
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
    // no serde attributes at all, so it must carry no serde derive either.
    let serde = match contract.deser() {
        DeserStrategy::Derive => quote! { , serde::Serialize, serde::Deserialize },
        DeserStrategy::HandWrittenBuffered { .. } | DeserStrategy::HandWrittenFieldless => {
            quote! {}
        }
    };
    quote! { #[derive(#(#names),* #serde)] }
}

/// A type reference, spelled out.
pub(super) fn type_ref(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    reference(ty, contracts, config, false)
}

/// The same type, named from outside the `types` module.
///
/// A named type renders as a bare identifier inside `types.rs` and must not anywhere else: the
/// client module re-exports `Error` from the support module, and a document with a schema called
/// `Error` — the petstore has one — would otherwise produce `Error<Error>` whose two `Error`s are
/// different types. The bug is silent, because it still compiles.
pub(crate) fn type_path(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    reference(ty, contracts, config, true)
}

/// A response payload named from outside the `types` module.
pub(crate) fn response_type_path(
    body: &ResponseBody,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    match body {
        ResponseBody::Json(ty) => type_path(ty, contracts, config),
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

fn enum_type_ref(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    enum_reference(ty, contracts, config, false)
}

fn enum_reference(
    ty: &TypeRef,
    contracts: &Contracts,
    config: &Config,
    qualified: bool,
) -> TokenStream {
    let rendered = reference(ty, contracts, config, qualified);
    if enum_type_is_boxed(ty) {
        quote! { Box<#rendered> }
    } else {
        rendered
    }
}

fn reference(ty: &TypeRef, contracts: &Contracts, config: &Config, qualified: bool) -> TokenStream {
    let type_ref = |inner: &TypeRef| reference(inner, contracts, config, qualified);
    match ty {
        TypeRef::Named(index) => {
            if let Some(contract) = contracts.get(*index) {
                let name = ident(contract.rust_name());
                if contract.docs().deprecated && qualified {
                    quote! { super::types::__progeny_deprecated::#name }
                } else if contract.docs().deprecated {
                    quote! { __progeny_deprecated::#name }
                } else if qualified {
                    quote! { super::types::#name }
                } else {
                    quote! { #name }
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
        TypeRef::Value => quote! { serde_json::Value },
        TypeRef::Option(inner) => {
            let inner = type_ref(inner);
            quote! { Option<#inner> }
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
    let deprecated = docs.deprecated.then(|| quote! { #[deprecated] });
    quote! { #(#attributes)* #deprecated }
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
