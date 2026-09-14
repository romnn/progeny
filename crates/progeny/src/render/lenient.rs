//! The impls a read form carries: the lenient decoder, `Deserialize` through it, and the
//! conversion from the strict twin.
//!
//! Like the serde impls, everything here transcribes a contract record: which members a
//! struct has and what each declares about its presence, which variants a union has and
//! what names them, which type a newtype wraps.
//! The one thing decided here is nothing — the form was decided by [`crate::contract`],
//! and this module only reads which one a contract is.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::serde_impl::implementation_type;
use super::types::{Scope, enum_type_is_boxed, type_ref_in};
use crate::config::{Config, UnknownFields};
use crate::contract::{
    Capture, ContractKind, Contracts, FieldContract, Form, Presence, RustIdent, SkipRule,
    TypeContract, TypeRef,
};

/// Every impl a contract's form asks for beyond its serde ones.
///
/// A shared type gets the lenient decoder beside its strict serde impls.
/// A read form gets the decoder, a `Deserialize` that goes through it, and — when it is a
/// twin — `From` its strict half.
/// A strict type gets nothing here.
pub(super) fn impls(
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let scope = Scope::of(contract);
    match contract.form() {
        Form::Strict { .. } => TokenStream::new(),
        Form::Shared => {
            let own_site = own_site(contract, scope);
            let decoder = decoder(contract, contracts, config, scope);
            quote! { #own_site #decoder }
        }
        Form::Lenient { strict } => {
            let own_site = own_site(contract, scope);
            let decoder = decoder(contract, contracts, config, scope);
            let deserialize = deserialize(contract, scope);
            let from = strict
                .map(|strict| from_strict(contract, strict, contracts, config, scope))
                .unwrap_or_default();
            quote! { #own_site #decoder #deserialize #from }
        }
    }
}

/// The type's own site as an associated constant, so a caller can ask a report about it.
///
/// The decoder's own literals name the same site; this is the spelling a consumer reaches,
/// because the origin pointer in a site is the description's and nothing else in the
/// generated crate says it.
/// An alias has no impls to hang it on.
fn own_site(contract: &TypeContract, scope: Scope) -> TokenStream {
    if matches!(contract.kind(), ContractKind::Alias { .. }) {
        return TokenStream::new();
    }
    let support = scope.support();
    let name = implementation_type(contract);
    let literal = site_literal(contract, None, scope);
    quote! {
        impl #name {
            /// Where a report records what this type itself tolerated.
            ///
            /// A member's site is this with the member's wire name:
            /// `Site { member: Some("name"), ..Self::SITE }`.
            /// What `Degradations::at` and `Degradations::touches` are asked with, and the
            /// root `Lenient::decode` takes when a payload is this type.
            pub const SITE: #support::Site = #literal;
        }
    }
}

/// The `Lenient` impl: how the type reads itself out of buffered content.
fn decoder(
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let support = scope.support();
    let name = implementation_type(contract);
    let body = match contract.kind() {
        ContractKind::Struct { fields } => struct_body(contract, fields, scope),
        ContractKind::StringEnum { fallback, .. } => string_enum_body(contract, fallback, scope),
        ContractKind::Enum { variants, fallback } => {
            untagged_body(contract, variants, fallback, contracts, config, scope)
        }
        ContractKind::TaggedEnum {
            tag,
            variants,
            fallback,
        } => tagged_body(
            contract, tag, variants, fallback, true, contracts, config, scope,
        ),
        ContractKind::CarriedTagEnum {
            tag,
            variants,
            fallback,
        } => tagged_body(
            contract, tag, variants, fallback, false, contracts, config, scope,
        ),
        ContractKind::Newtype { inner } => {
            let inner = type_ref_in(inner, contracts, config, scope);
            quote! {
                <#inner as #support::Lenient<'de>>::lenient(content, site, report).map(Self)
            }
        }
        ContractKind::Tuple { items } => {
            let types = items
                .iter()
                .map(|item| type_ref_in(item, contracts, config, scope));
            let bindings: Vec<proc_macro2::Ident> = (0..items.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            quote! {
                let (#(#bindings,)*) =
                    <(#(#types,)*) as #support::Lenient<'de>>::lenient(content, site, report)?;
                Ok(Self(#(#bindings),*))
            }
        }
        // A name for another type reads as that type; there is nothing to implement.
        ContractKind::Alias { .. } => return TokenStream::new(),
    };
    // A struct names its own sites — one per member, plus its own for the leftovers — so the
    // site it was handed goes unused; the binding stays so every body reads the same.
    let site = if matches!(contract.kind(), ContractKind::Struct { .. }) {
        quote! { _site }
    } else {
        quote! { site }
    };
    quote! {
        impl<'de> #support::Lenient<'de> for #name {
            fn lenient(
                content: #support::Content<'de>,
                #site: &'static #support::Site,
                report: &mut #support::Degradations,
            ) -> Result<Self, #support::Problem> {
                #body
            }
        }
    }
}

/// A struct: every declared member taken by its site, then the leftovers.
fn struct_body(contract: &TypeContract, fields: &[FieldContract], scope: Scope) -> TokenStream {
    let support = scope.support();
    let literal_name = contract.rust_name().as_str();
    let own = site(contract, None, scope);
    let denied = contract.unknown_fields() == UnknownFields::Deny;
    let reads = fields.iter().map(|field| {
        let member = ident(&field.rust_name);
        let deprecated = deprecated_field(field, contract.docs().deprecated);
        if let Some(capture) = field.capture {
            let declared = capture == Capture::Declared;
            return quote! {
                #deprecated
                #member: members.rest(#own, report, #declared, #denied),
            };
        }
        let at = site(contract, Some(field.wire_name.as_str()), scope);
        if field.skip_serializing_if == SkipRule::WhenOmitted {
            return quote! { #deprecated #member: members.take_presence(#at, report), };
        }
        // A member declared `type: null` is `null` on every well-formed response, so `null` is
        // what it allows whatever else the description says about it.
        let only_null = matches!(&field.ty, TypeRef::Unit)
            || matches!(&field.ty, TypeRef::Option(inner) if **inner == TypeRef::Unit);
        let declared = match (field.presence, only_null) {
            (Presence::Required, false) => quote! { Required },
            (Presence::Optional, false) => quote! { Optional },
            (Presence::Required | Presence::Nullable, true) | (Presence::Nullable, false) => {
                quote! { Nullable }
            }
            (Presence::Optional | Presence::OptionalNullable, true)
            | (Presence::OptionalNullable, false) => quote! { OptionalNullable },
        };
        quote! {
            #deprecated
            #member: members.take(#at, #support::Declared::#declared, report),
        }
    });
    // A type without a capture member still says what it left out.
    let leftovers = (!fields.iter().any(FieldContract::is_capture))
        .then(|| quote! { members.report_rest(#own, report, #denied); });
    // `mut` only when a declared member is taken out: the capture map and the leftovers report
    // consume the reader, and an unneeded `mut` is a warning in the consumer's build.
    let binding = if fields.iter().any(|field| !field.is_capture()) {
        quote! { let mut members }
    } else {
        quote! { let members }
    };
    quote! {
        #binding = #support::Members::of(content, #literal_name)?;
        let value = Self { #(#reads)* };
        #leftovers
        Ok(value)
    }
}

/// A string enum: read through its own open `Deserialize`, with an unlisted value reported.
fn string_enum_body(contract: &TypeContract, fallback: &RustIdent, scope: Scope) -> TokenStream {
    let support = scope.support();
    let fallback = ident(fallback);
    let arm = if contract.docs().deprecated {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated decoder must match this deprecated variant"
            )]
            Self::#fallback(raw) => Some(raw.as_str()),
        }
    } else {
        quote! { Self::#fallback(raw) => Some(raw.as_str()), }
    };
    quote! {
        #support::open_string(content, site, report, |value| match value {
            #arm
            _ => None,
        })
    }
}

/// An untagged union: the first variant the payload fits strictly, read leniently.
fn untagged_body(
    contract: &TypeContract,
    variants: &[crate::contract::VariantContract],
    fallback: &RustIdent,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let support = scope.support();
    let probes = variants.iter().map(|variant| {
        let name = ident(&variant.rust_name);
        let ty = type_ref_in(&variant.ty, contracts, config, scope);
        let wrap = wrapped(&variant.ty, contract.docs().deprecated, &name);
        quote! {
            if let Some(value) = #support::probe::<#ty>(&content, site, report) {
                return Ok(#wrap);
            }
        }
    });
    let unknown = unknown_arm(contract, fallback);
    quote! {
        #(#probes)*
        #support::unknown_union(content, site, report, None).map(#unknown)
    }
}

/// A tagged union: the variant the tag names, read leniently, or the open arm.
#[expect(
    clippy::too_many_arguments,
    reason = "a tagged union's decoder is the product of its contract, its two tag styles and \
              the rendering scope; a struct would put the same values one indirection away"
)]
fn tagged_body(
    contract: &TypeContract,
    tag: &str,
    variants: &[crate::contract::TaggedVariant],
    fallback: &RustIdent,
    consumed: bool,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let support = scope.support();
    let literal_name = contract.rust_name().as_str();
    let wire_names: Vec<&str> = variants
        .iter()
        .map(|variant| variant.tag_value.as_str())
        .collect();
    // A consumed tag is the union's member, not the variant's: taken off before the variant
    // reads the rest, or it would be reported as a member the variant never declared.
    let payload = if consumed {
        quote! { #support::Members::of(content, #literal_name)?.without(TAG) }
    } else {
        quote! { content }
    };
    let arms = variants.iter().enumerate().map(|(index, variant)| {
        let name = ident(&variant.rust_name);
        let ty = type_ref_in(&variant.ty, contracts, config, scope);
        let wrap = wrapped(&variant.ty, contract.docs().deprecated, &name);
        quote! {
            Some(#index) => <#ty as #support::Lenient<'de>>::lenient(#payload, site, report)
                .map(|value| #wrap),
        }
    });
    let unknown = unknown_arm(contract, fallback);
    quote! {
        const TAG: &str = #tag;
        const VARIANTS: &[&str] = &[#(#wire_names),*];
        match #support::tag_choice(&content, TAG, VARIANTS)? {
            #(#arms)*
            _ => {
                let tag = #support::tag_text(&content, TAG).map(str::to_owned);
                #support::unknown_union(content, site, report, tag.as_deref()).map(#unknown)
            }
        }
    }
}

/// A read value placed in its variant, boxed the way the enum declares it.
fn wrapped(ty: &TypeRef, deprecated: bool, variant: &proc_macro2::Ident) -> TokenStream {
    let value = if enum_type_is_boxed(ty) {
        quote! { Box::new(value) }
    } else {
        quote! { value }
    };
    if deprecated {
        quote! {
            {
                #[expect(
                    deprecated,
                    reason = "the generated decoder must construct this deprecated variant"
                )]
                let resolved = Self::#variant(#value);
                resolved
            }
        }
    } else {
        quote! { Self::#variant(#value) }
    }
}

/// The closure that puts an unrecognized payload in the open arm.
pub(super) fn unknown_arm(contract: &TypeContract, fallback: &RustIdent) -> TokenStream {
    let fallback = ident(fallback);
    if contract.docs().deprecated {
        quote! {
            |value| {
                #[expect(
                    deprecated,
                    reason = "the generated decoder must construct this deprecated variant"
                )]
                let resolved = Self::#fallback(Box::new(value));
                resolved
            }
        }
    } else {
        quote! { |value| Self::#fallback(Box::new(value)) }
    }
}

/// `Deserialize` for a read form: the lenient decode, with the report set aside.
///
/// What a caller who reaches for `serde_json::from_str` on a read type gets: the value
/// the generated client would have produced, minus the report the client returns beside it.
/// The report is reachable through `Lenient::decode` for a caller who wants it.
/// A string enum's own `Deserialize` is already open and already what a lenient read
/// produces, so it keeps it; an alias has no impls.
fn deserialize(contract: &TypeContract, scope: Scope) -> TokenStream {
    if matches!(
        contract.kind(),
        ContractKind::StringEnum { .. } | ContractKind::Alias { .. }
    ) {
        return TokenStream::new();
    }
    let support = scope.support();
    let name = implementation_type(contract);
    let root = site(contract, None, scope);
    quote! {
        impl<'de> serde::Deserialize<'de> for #name {
            /// The lenient decode, with the degradation report discarded; decode through
            /// `Lenient::decode` to keep it.
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                <Self as #support::Lenient<'de>>::decode(deserializer, #root)
                    .map(|decoded| decoded.value)
            }
        }
    }
}

/// `From` the strict twin: every required member wrapped in `Some`, every twinned type inside
/// converted, and nothing captured.
fn from_strict(
    contract: &TypeContract,
    strict: crate::contract::TypeIndex,
    contracts: &Contracts,
    config: &Config,
    scope: Scope,
) -> TokenStream {
    let name = implementation_type(contract);
    let strict_name = type_ref_in(&TypeRef::Named(strict), contracts, config, scope);
    let Some(strict) = contracts.get(strict) else {
        return TokenStream::new();
    };
    let body = match (contract.kind(), strict.kind()) {
        (
            ContractKind::Struct { fields },
            ContractKind::Struct {
                fields: strict_fields,
            },
        ) => struct_conversion(contract, fields, strict_fields, contracts),
        (
            ContractKind::Enum {
                variants, fallback, ..
            },
            ContractKind::Enum {
                variants: strict_variants,
                ..
            },
        ) => {
            let pairs = variants
                .iter()
                .zip(strict_variants)
                .map(|(variant, source)| (&variant.rust_name, &variant.ty, &source.ty));
            variant_conversions(pairs, fallback, &strict_name, contracts)
        }
        (
            ContractKind::TaggedEnum {
                variants, fallback, ..
            }
            | ContractKind::CarriedTagEnum {
                variants, fallback, ..
            },
            ContractKind::TaggedEnum {
                variants: strict_variants,
                ..
            }
            | ContractKind::CarriedTagEnum {
                variants: strict_variants,
                ..
            },
        ) => {
            let pairs = variants
                .iter()
                .zip(strict_variants)
                .map(|(variant, source)| (&variant.rust_name, &variant.ty, &source.ty));
            variant_conversions(pairs, fallback, &strict_name, contracts)
        }
        (ContractKind::Newtype { .. }, ContractKind::Newtype { inner }) => {
            let converted = convert(quote! { value.0 }, inner, contracts);
            quote! { Self(#converted) }
        }
        (ContractKind::Tuple { .. }, ContractKind::Tuple { items }) => {
            let bindings: Vec<proc_macro2::Ident> = (0..items.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            let converted = items
                .iter()
                .zip(&bindings)
                .map(|(item, binding)| convert(quote! { #binding }, item, contracts));
            quote! {
                let #strict_name(#(#bindings),*) = value;
                Self(#(#converted),*)
            }
        }
        // A string enum never twins, an alias has no impls, and a twin always has its strict
        // half's kind; nothing else can arrive.
        _ => return TokenStream::new(),
    };
    // No expectation on the impl itself: both type paths go through the non-deprecated aliases,
    // and each member that is deprecated carries its own, so one here would go unfulfilled.
    quote! {
        impl From<#strict_name> for #name {
            /// The strict value as a read value: every required member present, nothing
            /// undeclared.
            /// Infallible, so tests and mock servers can build strict values and answer
            /// with them.
            fn from(value: #strict_name) -> Self {
                #body
            }
        }
    }
}

/// A struct's conversion: every member from its strict counterpart, the required ones wrapped,
/// and the capture member the read form added left empty.
fn struct_conversion(
    contract: &TypeContract,
    fields: &[FieldContract],
    strict_fields: &[FieldContract],
    contracts: &Contracts,
) -> TokenStream {
    let members = fields.iter().map(|field| {
        let member = ident(&field.rust_name);
        let deprecated = deprecated_field(field, contract.docs().deprecated);
        let Some(source) = strict_fields
            .iter()
            .find(|strict_field| strict_field.rust_name == field.rust_name)
        else {
            // The capture member the read form added: a strict value has nothing undeclared to
            // bring along.
            return quote! { #deprecated #member: Default::default(), };
        };
        let converted = convert(quote! { value.#member }, &source.ty, contracts);
        if field.presence == Presence::Required && !field.is_capture() {
            quote! { #deprecated #member: Some(#converted), }
        } else {
            quote! { #deprecated #member: #converted, }
        }
    });
    quote! { Self { #(#members)* } }
}

/// A union's conversion: each variant's payload converted where its type twins, and the open
/// arm carried over as it is.
fn variant_conversions<'a>(
    pairs: impl Iterator<Item = (&'a RustIdent, &'a TypeRef, &'a TypeRef)>,
    fallback: &RustIdent,
    strict_name: &TokenStream,
    contracts: &Contracts,
) -> TokenStream {
    let arms = pairs.map(|(name, read, strict)| {
        let name = ident(name);
        let converted = boxed_convert(strict, read, contracts);
        quote! { #strict_name::#name(value) => Self::#name(#converted), }
    });
    let fallback = ident(fallback);
    quote! {
        match value {
            #(#arms)*
            #strict_name::#fallback(value) => Self::#fallback(value),
        }
    }
}

/// A boxed variant payload, converted if its type twins.
fn boxed_convert(strict: &TypeRef, read: &TypeRef, contracts: &Contracts) -> TokenStream {
    if !needs_conversion(strict, contracts) {
        return quote! { value };
    }
    if enum_type_is_boxed(read) {
        // Unboxed into a binding first: a method chain would bind tighter than a bare deref,
        // and parentheses around a deref that needs none are a lint in the consumer's build.
        let inner = convert(quote! { unboxed }, strict, contracts);
        quote! {
            {
                let unboxed = *value;
                Box::new(#inner)
            }
        }
    } else {
        convert(quote! { value }, strict, contracts)
    }
}

/// Whether a strict type reference names a twinned type anywhere inside.
fn needs_conversion(ty: &TypeRef, contracts: &Contracts) -> bool {
    let mut reached = Vec::new();
    ty.named(&mut reached);
    reached.iter().any(|index| {
        contracts
            .get(*index)
            .is_some_and(|contract| matches!(contract.form(), Form::Strict { twin: Some(_) }))
    })
}

/// An expression converting `value`, of the strict type `ty`, to the read form of `ty`.
fn convert(value: TokenStream, ty: &TypeRef, contracts: &Contracts) -> TokenStream {
    if !needs_conversion(ty, contracts) {
        return value;
    }
    match ty {
        // An alias has no `From` of its own: the conversion is its target's.
        TypeRef::Named(index) => {
            if let Some(ContractKind::Alias { target }) =
                contracts.get(*index).map(TypeContract::kind)
            {
                convert(value, target, contracts)
            } else {
                quote! { ::std::convert::Into::into(#value) }
            }
        }
        // Each maps over what it holds: an `Option`, a `Presence`, a fixed array.
        TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Array(inner, _) => {
            let mapper = mapper(inner, contracts);
            quote! { #value.map(#mapper) }
        }
        TypeRef::Vec(inner) => {
            let mapper = mapper(inner, contracts);
            quote! { #value.into_iter().map(#mapper).collect() }
        }
        TypeRef::Map(inner) => {
            let inner = convert(quote! { inner }, inner, contracts);
            quote! { #value.into_iter().map(|(key, inner)| (key, #inner)).collect() }
        }
        TypeRef::Boxed(inner) => {
            let inner = convert(quote! { unboxed }, inner, contracts);
            quote! {
                {
                    let unboxed = *#value;
                    Box::new(#inner)
                }
            }
        }
        TypeRef::Tuple(items) => {
            let bindings: Vec<proc_macro2::Ident> = (0..items.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            let converted = items
                .iter()
                .zip(&bindings)
                .map(|(item, binding)| convert(quote! { #binding }, item, contracts));
            quote! {
                {
                    let (#(#bindings,)*) = #value;
                    (#(#converted,)*)
                }
            }
        }
        // Nothing named inside, so `needs_conversion` was false and this arm is unreachable.
        TypeRef::Unit
        | TypeRef::Bool
        | TypeRef::I64
        | TypeRef::U64
        | TypeRef::F64
        | TypeRef::String
        | TypeRef::Format(_)
        | TypeRef::Upload
        | TypeRef::Value => value,
    }
}

/// The function that converts one element of a container: `Into::into` itself where that is
/// the whole conversion, or a closure where the element is a container in turn.
///
/// The bare path rather than a closure around it, because a closure that only forwards to a
/// function is a lint in the consumer's build.
fn mapper(element: &TypeRef, contracts: &Contracts) -> TokenStream {
    let converted = convert(quote! { element }, element, contracts);
    let direct = quote! { ::std::convert::Into::into(element) };
    if converted.to_string() == direct.to_string() {
        quote! { ::std::convert::Into::into }
    } else {
        quote! { |element| #converted }
    }
}

/// A reference to the site literal for a contract, or one of its members.
///
/// A struct literal rather than a constructor call, because a reference to a constant struct
/// literal is promoted to `'static` and a reference to a call is not.
pub(super) fn site(contract: &TypeContract, member: Option<&str>, scope: Scope) -> TokenStream {
    let literal = site_literal(contract, member, scope);
    quote! { &#literal }
}

/// The site literal itself.
fn site_literal(contract: &TypeContract, member: Option<&str>, scope: Scope) -> TokenStream {
    let support = scope.support();
    let type_name = contract.rust_name().as_str();
    let origin = contract.origin().to_string();
    let member = member.map_or_else(|| quote! { None }, |member| quote! { Some(#member) });
    quote! {
        #support::Site {
            type_name: #type_name,
            origin: #origin,
            member: #member,
        }
    }
}

/// An expectation on the exact statement that touches a deprecated contract member.
fn deprecated_field(field: &FieldContract, contract_deprecated: bool) -> TokenStream {
    if contract_deprecated || field.docs.deprecated {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated decoder must fill this deprecated contract member"
            )]
        }
    } else {
        TokenStream::new()
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
