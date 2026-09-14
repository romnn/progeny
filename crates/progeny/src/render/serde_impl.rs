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

use super::types::Scope;
use crate::config::UnknownFields;
use crate::contract::{ContractKind, Contracts, DeserStrategy, RustIdent, SkipRule, TypeContract};

/// Whether any type takes a hand-written path, and therefore whether the support module is needed.
pub(super) fn needed(contracts: &Contracts) -> bool {
    contracts
        .types()
        .iter()
        .any(|contract| contract.deser() != DeserStrategy::Derive)
}

/// The hand-written serde impls of one type.
///
/// A read form's `Deserialize` is the lenient decoder's, rendered by [`super::lenient`], so a
/// read form on a hand-written path gets its `Serialize` half from here and nothing else. The
/// string enum is the exception: its one open implementation is already what a lenient read
/// produces, so it keeps both halves in every form.
pub(super) fn one(contract: &TypeContract) -> TokenStream {
    let serialize_only = contract.form().is_lenient();
    match (contract.deser(), contract.kind()) {
        (DeserStrategy::HandWrittenBuffered { unknown }, ContractKind::Struct { fields }) => {
            buffered(contract, fields, unknown, serialize_only)
        }
        (DeserStrategy::HandWrittenFieldless, ContractKind::StringEnum { variants, fallback }) => {
            fieldless(contract, variants, fallback)
        }
        (
            DeserStrategy::HandWrittenCarriedTag,
            ContractKind::CarriedTagEnum {
                tag,
                variants,
                fallback,
            },
        ) => carried(contract, tag, variants, fallback, serialize_only),
        // Every other pairing is the derive's, and the eligibility function is what guarantees
        // that: a hand-written strategy on a kind with no implementation here would be a ruling
        // this module never saw.
        _ => quote! {},
    }
}

/// The non-deprecated path generated impls use for a deprecated public contract.
pub(super) fn implementation_type(contract: &TypeContract) -> TokenStream {
    let name = ident(contract.rust_name());
    if contract.docs().deprecated {
        quote! { __progeny_deprecated::#name }
    } else {
        quote! { #name }
    }
}

fn buffered(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    unknown: UnknownFields,
    serialize_only: bool,
) -> TokenStream {
    let name = implementation_type(contract);
    let reading = (!serialize_only).then(|| reading(contract, fields, unknown));
    let writing = writing(contract, fields);
    quote! {
        #reading

        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                #writing
            }
        }
    }
}

/// The `Assemble` and `Deserialize` halves: what the buffer holds and how it becomes the struct.
fn reading(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    unknown: UnknownFields,
) -> TokenStream {
    let name = implementation_type(contract);
    let support = Scope::of(contract).support();
    let literal_name = contract.rust_name().as_str();
    // The capture member has no wire name: it is the leftovers, read after the declared names
    // and never looked for among them.
    let declared: Vec<&crate::contract::FieldContract> =
        fields.iter().filter(|field| !field.is_capture()).collect();
    let wire_names: Vec<&str> = declared
        .iter()
        .map(|field| field.wire_name.as_str())
        .collect();
    // Resolved by the eligibility ruling and carried in the strategy, so this module has no
    // policy to decide.
    let unknown = match unknown {
        UnknownFields::Ignore => quote! { Ignore },
        UnknownFields::Deny => quote! { Deny },
        UnknownFields::Capture => quote! { Capture },
    };

    let defaulted = declared
        .iter()
        // A presence-preserving field uses its own `Default` only to represent an absent member.
        // OpenAPI's declared `default` remains documentation about the server and never enters
        // this decision — see `types::with_default`.
        .map(|field| field.skip_serializing_if == SkipRule::WhenOmitted);
    let reads = fields.iter().map(|field| {
        let member = ident(&field.rust_name);
        let wire = field.wire_name.as_str();
        let deprecated = deprecated_field(field, contract.docs().deprecated);
        if field.is_capture() {
            quote! { #deprecated #member: buffer.take_rest()?, }
        } else if field.skip_serializing_if == SkipRule::WhenOmitted {
            quote! {
                #deprecated
                #member: #support::take_presence_or_default(buffer, #wire)?,
            }
        } else {
            quote! { #deprecated #member: buffer.take(#wire)?, }
        }
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
        impl<'de> #support::Assemble<'de> for #name {
            const NAME: &'static str = #literal_name;
            const FIELDS: &'static [&'static str] = &[#(#wire_names),*];
            const DEFAULTED: &'static [bool] = &[#(#defaulted),*];

            fn assemble<E>(#buffer_binding: &mut #support::Buffer<'de>) -> Result<Self, E>
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
                use #support::Assemble as _;
                serde::Deserializer::deserialize_struct(
                    deserializer,
                    Self::NAME,
                    Self::FIELDS,
                    #support::BufferVisitor::<Self>::new(
                        #support::Unknown::#unknown,
                    ),
                )
            }
        }
    }
}

/// The `Serialize` body: the member count, then one write per member.
///
/// A struct with a capture member writes as a map instead — its keys are only known at run time,
/// and `serialize_struct` takes static names — which is the same wire the derive's `flatten`
/// produces.
fn writing(contract: &TypeContract, fields: &[crate::contract::FieldContract]) -> TokenStream {
    if let Some(capture) = fields.iter().find(|field| field.is_capture()) {
        return writing_map(contract, fields, capture);
    }
    let literal_name = contract.rust_name().as_str();
    // The count a struct is serialized with has to match what is actually written, so a skipped
    // member is subtracted from it rather than assumed away.
    let always = fields
        .iter()
        .filter(|field| field.skip_serializing_if == SkipRule::Never)
        .count();
    let conditional: Vec<TokenStream> = fields
        .iter()
        .filter(|field| field.skip_serializing_if != SkipRule::Never)
        .map(|field| {
            let member = ident(&field.rust_name);
            let deprecated = deprecated_field(field, contract.docs().deprecated);
            match field.skip_serializing_if {
                SkipRule::Never => quote! {},
                SkipRule::WhenNone => {
                    quote! { #deprecated if self.#member.is_some() { count += 1; } }
                }
                SkipRule::WhenOmitted => {
                    quote! { #deprecated if !self.#member.is_omitted() { count += 1; } }
                }
            }
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
        let deprecated = deprecated_field(field, contract.docs().deprecated);
        match field.skip_serializing_if {
            SkipRule::Never => {
                quote! { #deprecated state.serialize_field(#wire, &self.#member)?; }
            }
            SkipRule::WhenNone => quote! {
                #deprecated
                if self.#member.is_some() {
                    state.serialize_field(#wire, &self.#member)?;
                } else {
                    state.skip_field(#wire)?;
                }
            },
            SkipRule::WhenOmitted => quote! {
                #deprecated
                if self.#member.is_omitted() {
                    state.skip_field(#wire)?;
                } else {
                    state.serialize_field(#wire, &self.#member)?;
                }
            },
        }
    });

    quote! {
        use serde::ser::SerializeStruct as _;
        #binding
        #(#conditional)*
        #state = serializer.serialize_struct(#literal_name, count)?;
        #(#writes)*
        state.end()
    }
}

/// The `Serialize` body of a struct with a capture member: every declared member as an entry,
/// then every captured one.
///
/// A captured entry whose key names a declared member is left out: the declared member is the
/// type's statement about that key, and writing both would put the same key on the wire twice.
fn writing_map(
    contract: &TypeContract,
    fields: &[crate::contract::FieldContract],
    capture: &crate::contract::FieldContract,
) -> TokenStream {
    let declared: Vec<&crate::contract::FieldContract> =
        fields.iter().filter(|field| !field.is_capture()).collect();
    let wire_names: Vec<&str> = declared
        .iter()
        .map(|field| field.wire_name.as_str())
        .collect();
    let writes = declared.iter().map(|field| {
        let member = ident(&field.rust_name);
        let wire = field.wire_name.as_str();
        let deprecated = deprecated_field(field, contract.docs().deprecated);
        match field.skip_serializing_if {
            SkipRule::Never => {
                quote! { #deprecated state.serialize_entry(#wire, &self.#member)?; }
            }
            SkipRule::WhenNone => quote! {
                #deprecated
                if self.#member.is_some() {
                    state.serialize_entry(#wire, &self.#member)?;
                }
            },
            SkipRule::WhenOmitted => quote! {
                #deprecated
                if !self.#member.is_omitted() {
                    state.serialize_entry(#wire, &self.#member)?;
                }
            },
        }
    });
    let captured = ident(&capture.rust_name);
    let deprecated = deprecated_field(capture, contract.docs().deprecated);
    quote! {
        use serde::ser::SerializeMap as _;
        const DECLARED: &[&str] = &[#(#wire_names),*];
        let mut state = serializer.serialize_map(None)?;
        #(#writes)*
        #deprecated
        for (key, value) in &self.#captured {
            if !DECLARED.contains(&key.as_str()) {
                state.serialize_entry(key, value)?;
            }
        }
        state.end()
    }
}

/// An expectation on the exact statement that touches a deprecated contract member.
fn deprecated_field(
    field: &crate::contract::FieldContract,
    contract_deprecated: bool,
) -> TokenStream {
    if contract_deprecated || field.docs.deprecated {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated serializer must preserve this deprecated contract member"
            )]
        }
    } else {
        TokenStream::new()
    }
}

/// The impls of an open string enum: a string in, the same string out.
///
/// Read with `deserialize_str` and written with `serialize_str`, never through serde's enum
/// repertoire: the description says the wire carries a string, and a string it does not list is
/// the fallback variant holding that string — which serializes back to the exact bytes it was
/// read from. No buffering, so this works with formats that are not self-describing, and it is
/// the one implementation under both serde strategies.
fn fieldless(
    contract: &TypeContract,
    variants: &[crate::contract::StringVariant],
    fallback: &RustIdent,
) -> TokenStream {
    let name = implementation_type(contract);
    let support = Scope::of(contract).support();
    let literal_name = contract.rust_name().as_str();
    let fallback = ident(fallback);

    let resolve = variants.iter().map(|variant| {
        let member = ident(&variant.rust_name);
        let wire = variant.wire_name.as_str();
        if contract.docs().deprecated {
            quote! {
                #wire => {
                    #[expect(
                        deprecated,
                        reason = "the generated deserializer must construct this deprecated variant"
                    )]
                    let resolved = Self::#member;
                    Some(resolved)
                }
            }
        } else {
            quote! { #wire => Some(Self::#member), }
        }
    });
    let unlisted = if contract.docs().deprecated {
        quote! {
            |value| {
                #[expect(
                    deprecated,
                    reason = "the generated deserializer must construct this deprecated variant"
                )]
                let resolved = Self::#fallback(value);
                resolved
            }
        }
    } else {
        quote! { Self::#fallback }
    };

    quote! {
        impl<'de> serde::Deserialize<'de> for #name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                serde::Deserializer::deserialize_str(
                    deserializer,
                    #support::OpenVariants::new(
                        #literal_name,
                        |name| match name {
                            #(#resolve)*
                            _ => None,
                        },
                        #unlisted,
                    ),
                )
            }
        }

        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }
    }
}

/// The impls of a carried-tag enum, which exists only on this path — no serde encoding reads a
/// tag it leaves in the payload.
///
/// Deserializing buffers the payload, reads the tag member, and replays the whole payload — tag
/// included — into the named variant's type, which declares the member and keeps it. Serializing
/// is the variant as it is, for the same reason: the value already carries its tag. The match the
/// resolver runs binds its last variant with a `_` arm, sound because the support `dispatch`
/// hands back only positions inside `VARIANTS`.
fn carried(
    contract: &TypeContract,
    tag: &str,
    variants: &[crate::contract::TaggedVariant],
    fallback: &RustIdent,
    serialize_only: bool,
) -> TokenStream {
    let name = implementation_type(contract);
    let support = Scope::of(contract).support();
    let fallback_name = fallback;
    let fallback = ident(fallback);
    let wire_names: Vec<&str> = variants
        .iter()
        .map(|variant| variant.tag_value.as_str())
        .collect();

    let read = |variant: &crate::contract::TaggedVariant| {
        let member = ident(&variant.rust_name);
        if contract.docs().deprecated {
            quote! {
                {
                    #[expect(
                        deprecated,
                        reason = "the generated deserializer must construct this deprecated variant"
                    )]
                    let resolved = serde::Deserialize::deserialize(replay).map(Self::#member);
                    resolved
                }
            }
        } else {
            quote! { serde::Deserialize::deserialize(replay).map(Self::#member) }
        }
    };
    // One variant needs no match at all — and a `match` over a single `_` arm is a style lint in
    // the consumer's build about code they did not write.
    let resolver = if let [only] = variants {
        let body = read(only);
        quote! { |_, replay| #body }
    } else {
        let arms = variants.iter().enumerate().map(|(index, variant)| {
            let body = read(variant);
            if index + 1 == variants.len() {
                quote! { _ => #body, }
            } else {
                quote! { #index => #body, }
            }
        });
        quote! {
            |variant, replay| match variant {
                #(#arms)*
            }
        }
    };
    let write = carried_writes(contract, tag, variants, &support);
    let deprecated = contract.docs().deprecated.then(|| {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated serializer must match this deprecated variant"
            )]
        }
    });
    let unknown = super::lenient::unknown_arm(contract, fallback_name);

    let reading = (!serialize_only).then(|| {
        quote! {
            impl<'de> serde::Deserialize<'de> for #name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    const TAG: &str = #tag;
                    const VARIANTS: &[&str] = &[#(#wire_names),*];
                    let payload: #support::Content<'de> =
                        serde::Deserialize::deserialize(deserializer)?;
                    #support::dispatch(TAG, VARIANTS, payload, #resolver, #unknown)
                }
            }
        }
    });

    quote! {
        #reading

        impl serde::Serialize for #name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    #(#write)*
                    // The unrecognized payload, exactly as it arrived.
                    #deprecated
                    Self::#fallback(value) => serde::Serialize::serialize(value, serializer),
                }
            }
        }
    }
}

/// One `Serialize` arm per named variant of a carried-tag union.
fn carried_writes<'a>(
    contract: &'a TypeContract,
    tag: &'a str,
    variants: &'a [crate::contract::TaggedVariant],
    support: &'a TokenStream,
) -> impl Iterator<Item = TokenStream> + 'a {
    variants.iter().map(move |variant| {
        let member = ident(&variant.rust_name);
        let value = variant.tag_value.as_str();
        // Through the support injector rather than a bare delegation: the payload's own tag
        // member may be unset or stale in a hand-built value, and the wire must name the arm
        // the caller chose.
        let write = quote! {
            Self::#member(value) => {
                #support::serialize_carried(value, #tag, #value, serializer)
            }
        };
        if contract.docs().deprecated {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the generated serializer must match this deprecated variant"
                )]
                #write
            }
        } else {
            write
        }
    })
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
