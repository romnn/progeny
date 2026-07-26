//! Rendering types: a direct transcription of the contract records.
//!
//! **This module makes no decisions.** Every question it could ask — what a field is called on the
//! wire, whether an absent key is legal, which derives appear, how long a tuple is — was answered in
//! [`crate::contract`] and is sitting in the record it is handed. When the derive strategy is
//! selected the same record renders as `#[serde(...)]` attributes; when the hand-written strategy is
//! selected it renders as function bodies instead. Two renderings of one record, which is the whole
//! discipline in one sentence.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::config::{BytesRepr, Config, DateTimeCrate, MapKind, UuidCrate};
use crate::contract::{
    ContractKind, Contracts, DeserStrategy, FieldContract, RustIdent, SkipRule, Tagging,
    TypeContract, TypeRef,
};
use crate::shape::{Docs, Format};

/// Render every type in the contract set.
pub(super) fn render(contracts: &Contracts, config: &Config) -> TokenStream {
    let items = contracts
        .types()
        .iter()
        .map(|contract| one(contract, contracts, config));
    quote! { #(#items)* }
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
            let helpers = fields
                .iter()
                .filter_map(|field| default_helper(field, contract, contracts, config));
            quote! {
                #docs
                #derives
                #deny
                pub struct #name {
                    #(#members)*
                }
                #(#helpers)*
            }
        }
        ContractKind::Enum { variants } => {
            let tagging = match (contract.deser(), contract.tagging()) {
                (DeserStrategy::Derive, Tagging::Untagged) => quote! { #[serde(untagged)] },
                _ => quote! {},
            };
            let arms = variants.iter().map(|variant| {
                let name = ident(&variant.rust_name);
                let ty = type_ref(&variant.ty, contracts, config);
                quote! { #name(#ty), }
            });
            quote! {
                #docs
                #derives
                #tagging
                pub enum #name {
                    #(#arms)*
                }
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

fn member(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(&field.rust_name);
    let ty = type_ref(&field.ty, contracts, config);
    let docs = docs(&field.docs);
    if contract.deser() != DeserStrategy::Derive {
        // No serde attributes at all: the hand-written implementation reads the same contract and
        // does not consult attributes, so leaving them on would be a second source of truth.
        return quote! { #docs pub #name: #ty, };
    }

    let mut attributes = Vec::new();
    if field.flatten {
        attributes.push(quote! { flatten });
    } else if name != field.wire_name {
        let wire = &field.wire_name;
        attributes.push(quote! { rename = #wire });
    }
    if let Some(path) = default_path(field, contract, contracts, config) {
        attributes.push(quote! { default = #path });
    }
    if field.skip_serializing_if == SkipRule::WhenNone && !field.flatten {
        attributes.push(quote! { skip_serializing_if = "Option::is_none" });
    }
    let serde = (!attributes.is_empty()).then(|| quote! { #[serde(#(#attributes),*)] });
    quote! { #docs #serde pub #name: #ty, }
}

/// The name of the function that produces a field's declared default.
pub(super) fn default_path(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> Option<String> {
    default_helper(field, contract, contracts, config)
        .map(|_| helper_name(contract, field).to_string())
}

fn helper_name(contract: &TypeContract, field: &FieldContract) -> proc_macro2::Ident {
    format_ident!(
        "default_{}_{}",
        heck::ToSnakeCase::to_snake_case(contract.rust_name().as_str()),
        field.rust_name.as_str()
    )
}

/// The function a `#[serde(default = "...")]` points at.
///
/// A default is a statement about what the value is when the member is absent, so it has to be
/// rendered as a value rather than recorded and forgotten. Rendered only where the value has a
/// literal form; where it does not, the contract layer has already dropped it and said so.
fn default_helper(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> Option<TokenStream> {
    let default = field.default.as_ref()?;
    let literal = literal(default, &field.ty, contracts, config)?;
    let name = helper_name(contract, field);
    let ty = type_ref(&field.ty, contracts, config);
    Some(quote! {
        fn #name() -> #ty { #literal }
    })
}

/// A JSON value as a Rust expression of the given type.
fn literal(
    value: &serde_json::Value,
    ty: &TypeRef,
    contracts: &Contracts,
    config: &Config,
) -> Option<TokenStream> {
    match (ty, value) {
        (TypeRef::Option(inner), serde_json::Value::Null) => {
            let ty = type_ref(inner, contracts, config);
            Some(quote! { None::<#ty> })
        }
        (TypeRef::Option(inner), _) => {
            let inner = literal(value, inner, contracts, config)?;
            Some(quote! { Some(#inner) })
        }
        (TypeRef::Boxed(inner), _) => {
            let inner = literal(value, inner, contracts, config)?;
            Some(quote! { Box::new(#inner) })
        }
        (TypeRef::Bool, serde_json::Value::Bool(flag)) => Some(quote! { #flag }),
        (TypeRef::I64, serde_json::Value::Number(number)) => {
            let parsed = number.as_i64()?;
            Some(quote! { #parsed })
        }
        (TypeRef::U64, serde_json::Value::Number(number)) => {
            let parsed = number.as_u64()?;
            Some(quote! { #parsed })
        }
        (TypeRef::F64, serde_json::Value::Number(number)) => {
            let parsed = number.as_f64()?;
            Some(quote! { #parsed })
        }
        (TypeRef::String | TypeRef::Format(_), serde_json::Value::String(text)) => {
            Some(quote! { #text.to_owned() })
        }
        (TypeRef::Vec(inner), serde_json::Value::Array(items)) => {
            let elements = items
                .iter()
                .map(|item| literal(item, inner, contracts, config))
                .collect::<Option<Vec<_>>>()?;
            Some(quote! { vec![#(#elements),*] })
        }
        (TypeRef::Unit, serde_json::Value::Null) => Some(quote! { () }),
        // A named type's literal form would need its own field-by-field construction, and a
        // structured default is rare enough that the contract layer drops it with a diagnostic
        // instead.
        _ => None,
    }
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
        DeserStrategy::HandWrittenBuffered | DeserStrategy::HandWrittenFieldless => quote! {},
    };
    quote! { #[derive(#(#names),* #serde)] }
}

/// A type reference, spelled out.
pub(super) fn type_ref(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    match ty {
        TypeRef::Named(index) => {
            if let Some(contract) = contracts.get(*index) {
                let name = ident(contract.rust_name());
                quote! { #name }
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
            let inner = type_ref(inner, contracts, config);
            quote! { Option<#inner> }
        }
        TypeRef::Vec(inner) => {
            let inner = type_ref(inner, contracts, config);
            quote! { Vec<#inner> }
        }
        TypeRef::Map(inner) => {
            let inner = type_ref(inner, contracts, config);
            match config.map {
                MapKind::BTreeMap => quote! { std::collections::BTreeMap<String, #inner> },
                MapKind::HashMap => quote! { std::collections::HashMap<String, #inner> },
                MapKind::IndexMap => quote! { indexmap::IndexMap<String, #inner> },
            }
        }
        TypeRef::Array(inner, len) => {
            let inner = type_ref(inner, contracts, config);
            let len = usize::try_from(*len).unwrap_or(0);
            quote! { [#inner; #len] }
        }
        TypeRef::Tuple(items) => {
            let items = items.iter().map(|item| type_ref(item, contracts, config));
            quote! { (#(#items),*) }
        }
        TypeRef::Boxed(inner) => {
            let inner = type_ref(inner, contracts, config);
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
        Format::Base64 | Format::Binary => match config.formats.bytes {
            BytesRepr::Vec | BytesRepr::Bytes => quote! { String },
        },
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}

/// Doc comments, one attribute per line so a multi-line description reads as one.
fn docs(docs: &Docs) -> TokenStream {
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

/// Split a description into doc-comment lines.
///
/// Carriage returns are dropped and tabs are kept: a doc comment is markdown, and the only thing
/// that must not survive is a line break inside one attribute.
fn wrap(text: &str) -> Vec<String> {
    text.replace('\r', "")
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}
