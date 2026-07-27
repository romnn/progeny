//! The hand-written `Serialize`/`Deserialize` implementations.
//!
//! Two function bodies per type instead of the derive's nine, with the shared machinery
//! monomorphized once per crate in [`crate::support`]. The bodies are rendered from the same
//! contract the derive attributes would have been rendered from — that is what makes the choice an
//! implementation detail rather than a compatibility decision, and what the differential harness
//! checks.
//!
//! When a type takes this path it carries **no** `#[serde(...)]` attributes at all. That is forced,
//! not stylistic: they are helper attributes of the serde derive macros, so with no derive on the
//! item they do not resolve and the crate does not compile.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::config::UnknownFields;
use crate::contract::{ContractKind, Contracts, DeserStrategy, RustIdent, SkipRule, TypeContract};

/// Whether any type takes a hand-written path, and therefore whether the support module is needed.
pub(super) fn needed(contracts: &Contracts) -> bool {
    contracts
        .types()
        .iter()
        .any(|contract| contract.deser() != DeserStrategy::Derive)
}

pub(super) fn render(contracts: &Contracts) -> TokenStream {
    let items = contracts
        .types()
        .iter()
        .map(|contract| one(contract, contracts));
    quote! { #(#items)* }
}

fn one(contract: &TypeContract, contracts: &Contracts) -> TokenStream {
    match (contract.deser(), contract.kind()) {
        (DeserStrategy::HandWrittenBuffered, ContractKind::Struct { fields }) => {
            buffered(contract, fields, contracts)
        }
        (DeserStrategy::HandWrittenFieldless, ContractKind::StringEnum { variants }) => {
            fieldless(contract, variants)
        }
        // Every other pairing is the derive's, and the eligibility function is what guarantees
        // that: a hand-written strategy on a kind with no implementation here would be a ruling
        // this module never saw.
        _ => quote! {},
    }
}

/// Whether these impls touch anything deprecated, and so need the allowance on the item.
///
/// Three ways to touch one, and the corpus produced all three: the type itself is deprecated, so
/// naming it in `impl Serialize for …` is a use (`cloudflare`); a *member* is deprecated, so reading
/// or writing it is a use (`jellyfin`, `github-31`); or a member's type is (`okta`). The allowance
/// goes on the item for the same reason it does everywhere else in this renderer — a member-level
/// attribute does not cover the impl header that names the type.
fn allowance(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    contracts: &Contracts,
) -> TokenStream {
    if contract.docs().deprecated || fields.iter().any(|field| field.docs.deprecated) {
        return quote! { #[allow(deprecated)] };
    }
    super::types::deprecated_use(fields.iter().map(|field| &field.ty), contracts)
}

fn buffered(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    contracts: &Contracts,
) -> TokenStream {
    let allow = allowance(contract, fields, contracts);
    let name = ident(contract.rust_name());
    // Threaded in rather than wrapped around, because `reading` emits two impls and an attribute
    // written once outside would land on only the first of them.
    let reading = reading(contract, fields, &allow);
    let writing = writing(contract, fields);
    quote! {
        #reading

        #allow
        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use serde::ser::SerializeStruct as _;
                #writing
            }
        }
    }
}

/// The `Assemble` and `Deserialize` halves: what the buffer holds and how it becomes the struct.
fn reading(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    allow: &TokenStream,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let literal_name = contract.rust_name().as_str();
    let wire_names: Vec<&str> = fields
        .iter()
        .map(|field| field.wire_name.as_str())
        .collect();
    let unknown = match contract.unknown_fields() {
        UnknownFields::Deny => quote! { Deny },
        // `Capture` is ruled to the derive, so it cannot arrive here.
        UnknownFields::Ignore | UnknownFields::Capture => quote! { Ignore },
    };

    let defaulted = fields
        .iter()
        // Always false: a declared default is documentation about the server, not an instruction
        // to fill the member in on the way in — see `types::with_default`. The flag stays in the
        // shipped trait because the *sequence* form of a struct needs it to tell a short sequence
        // from a defaulted tail, and both serde paths have to answer that question the same way.
        .map(|_| false);
    let reads = fields.iter().map(|field| {
        let member = ident(&field.rust_name);
        let wire = field.wire_name.as_str();
        quote! { #member: buffer.take(#wire)?, }
    });
    // A struct with no members reads nothing out of the buffer, and an unused binding is a warning
    // in the consumer's build. `cloudflare`, `github-31` and `okta` all declare one — an object
    // with no properties is a perfectly ordinary thing for a document to say.
    let buffer_binding = if fields.is_empty() {
        format_ident!("_buffer")
    } else {
        format_ident!("buffer")
    };

    quote! {
        #allow
        impl<'de> super::support::Assemble<'de> for #name {
            const NAME: &'static str = #literal_name;
            const FIELDS: &'static [&'static str] = &[#(#wire_names),*];
            const DEFAULTED: &'static [bool] = &[#(#defaulted),*];

            fn assemble<E>(#buffer_binding: &mut super::support::Buffer<'de>) -> Result<Self, E>
            where
                E: serde::de::Error,
            {
                Ok(Self { #(#reads)* })
            }
        }

        #allow
        impl<'de> serde::Deserialize<'de> for #name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                use super::support::Assemble as _;
                serde::Deserializer::deserialize_struct(
                    deserializer,
                    Self::NAME,
                    Self::FIELDS,
                    super::support::BufferVisitor::<Self>::new(
                        super::support::Unknown::#unknown,
                    ),
                )
            }
        }
    }
}

/// The `Serialize` body: the member count, then one write per member.
fn writing(contract: &TypeContract, fields: &[crate::contract::FieldContract]) -> TokenStream {
    let literal_name = contract.rust_name().as_str();
    // The count a struct is serialized with has to match what is actually written, so a skipped
    // member is subtracted from it rather than assumed away.
    let always = fields
        .iter()
        .filter(|field| field.skip_serializing_if == SkipRule::Never)
        .count();
    let conditional: Vec<TokenStream> = fields
        .iter()
        .filter(|field| field.skip_serializing_if == SkipRule::WhenNone)
        .map(|field| {
            let member = ident(&field.rust_name);
            quote! { if self.#member.is_some() { count += 1; } }
        })
        .collect();
    // `mut` only when something mutates it. A struct whose members are all unconditional never
    // reaches the `count += 1` arm, and an unnecessary `mut` is a warning in the consumer's build
    // about code they did not write — which the corpus compile gate denies, and rightly.
    let binding = if conditional.is_empty() {
        quote! { let count = #always; }
    } else {
        quote! { let mut count = #always; }
    };
    // The same rule for the serializer's state: `serialize_field` is what borrows it mutably, so a
    // struct with no members never does, and `end` takes it by value either way.
    let state = if fields.is_empty() {
        quote! { let state }
    } else {
        quote! { let mut state }
    };
    let writes = fields.iter().map(|field| {
        let member = ident(&field.rust_name);
        let wire = field.wire_name.as_str();
        match field.skip_serializing_if {
            SkipRule::Never => quote! { state.serialize_field(#wire, &self.#member)?; },
            SkipRule::WhenNone => quote! {
                if self.#member.is_some() {
                    state.serialize_field(#wire, &self.#member)?;
                } else {
                    state.skip_field(#wire)?;
                }
            },
        }
    });

    quote! {
        #binding
        #(#conditional)*
        #state = serializer.serialize_struct(#literal_name, count)?;
        #(#writes)*
        state.end()
    }
}

fn fieldless(contract: &TypeContract, variants: &[crate::contract::StringVariant]) -> TokenStream {
    // A deprecated string enum — `okta` has one — is used by the impl header that names it. No
    // member types to consider here: a fieldless variant carries nothing.
    let allow = if contract.docs().deprecated {
        quote! { #[allow(deprecated)] }
    } else {
        TokenStream::new()
    };
    let name = ident(contract.rust_name());
    let literal_name = contract.rust_name().as_str();
    let wire_names: Vec<&str> = variants
        .iter()
        .map(|variant| variant.wire_name.as_str())
        .collect();

    let resolve = variants.iter().map(|variant| {
        let member = ident(&variant.rust_name);
        let wire = variant.wire_name.as_str();
        quote! { #wire => Some(Self::#member), }
    });
    let write = variants.iter().enumerate().map(|(index, variant)| {
        let member = ident(&variant.rust_name);
        let wire = variant.wire_name.as_str();
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        quote! {
            Self::#member => serializer.serialize_unit_variant(#literal_name, #index, #wire),
        }
    });

    quote! {
        #allow
        impl<'de> serde::Deserialize<'de> for #name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                const NAME: &str = #literal_name;
                const VARIANTS: &[&str] = &[#(#wire_names),*];
                serde::Deserializer::deserialize_enum(
                    deserializer,
                    NAME,
                    VARIANTS,
                    // Resolved from the identifier alone: no buffering, so this keeps working with
                    // formats that are not self-describing.
                    super::support::UnitVariants::new(NAME, VARIANTS, |name| match name {
                        #(#resolve)*
                        _ => None,
                    }),
                )
            }
        }

        #allow
        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    #(#write)*
                }
            }
        }
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
