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
    let items = contracts.types().iter().map(one);
    quote! { #(#items)* }
}

fn one(contract: &TypeContract) -> TokenStream {
    match (contract.deser(), contract.kind()) {
        (DeserStrategy::HandWrittenBuffered, ContractKind::Struct { fields }) => {
            buffered(contract, fields)
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

fn buffered(contract: &TypeContract, fields: &[crate::contract::FieldContract]) -> TokenStream {
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

    // The count a struct is serialized with has to match what is actually written, so a skipped
    // member is subtracted from it rather than assumed away.
    let always = fields
        .iter()
        .filter(|field| field.skip_serializing_if == SkipRule::Never)
        .count();
    let conditional = fields
        .iter()
        .filter(|field| field.skip_serializing_if == SkipRule::WhenNone)
        .map(|field| {
            let member = ident(&field.rust_name);
            quote! { if self.#member.is_some() { count += 1; } }
        });
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
        impl<'de> super::support::Assemble<'de> for #name {
            const NAME: &'static str = #literal_name;
            const FIELDS: &'static [&'static str] = &[#(#wire_names),*];
            const DEFAULTED: &'static [bool] = &[#(#defaulted),*];

            fn assemble<E>(buffer: &mut super::support::Buffer<'de>) -> Result<Self, E>
            where
                E: serde::de::Error,
            {
                Ok(Self { #(#reads)* })
            }
        }

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

        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use serde::ser::SerializeStruct as _;
                let mut count = #always;
                #(#conditional)*
                let mut state = serializer.serialize_struct(#literal_name, count)?;
                #(#writes)*
                state.end()
            }
        }
    }
}

fn fieldless(contract: &TypeContract, variants: &[crate::contract::StringVariant]) -> TokenStream {
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
