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
//! A keyed enum is the one exception: its `Serialize` is still derived, so its attributes resolve
//! against that derive, and only its `Deserialize` is written here.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use super::types::Scope;
use crate::config::UnknownFields;
use crate::contract::{
    Capture, ContractKind, Contracts, DeserStrategy, RustIdent, SkipRule, TagRoutes, TypeContract,
};

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
        (
            DeserStrategy::HandWrittenFieldless,
            ContractKind::StringEnum {
                variants, fallback, ..
            },
        ) => fieldless(contract, variants, fallback),
        (
            DeserStrategy::HandWrittenCarriedTag,
            ContractKind::CarriedTagEnum {
                tag,
                variants,
                routes,
                fallback,
            },
        ) => carried(contract, tag, variants, *routes, fallback, serialize_only),
        // A read form's `Deserialize` is the lenient decoder's, and `Serialize` is derived in both.
        (DeserStrategy::HandWrittenKeyed, ContractKind::KeyedEnum { variants, fallback }) => {
            if serialize_only {
                quote! {}
            } else {
                keyed(contract, variants, fallback)
            }
        }
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
/// then every captured one, or the flattened union's.
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
    let rest = if capture.capture == Some(Capture::Union) {
        let support = Scope::of(contract).support();
        quote! {
            #deprecated
            #support::serialize_flattened(&self.#captured, &mut state, DECLARED)?;
        }
    } else {
        quote! {
            #deprecated
            for (key, value) in &self.#captured {
                if !DECLARED.contains(&key.as_str()) {
                    state.serialize_entry(key, value)?;
                }
            }
        }
    };
    quote! {
        use serde::ser::SerializeMap as _;
        const DECLARED: &[&str] = &[#(#wire_names),*];
        let mut state = serializer.serialize_map(None)?;
        #(#writes)*
        #rest
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
/// included — into the named variant's type, which declares the member and keeps it.
/// A variant may be named by several spellings, or by a missing or `null` tag, and `VARIANTS`
/// lists every spelling of every variant, so the resolver matches each variant by all of its
/// positions.
/// Serializing writes the variant through the support injector, so the wire names the arm the
/// caller chose: as its one name, or, where the variant has several ways to be named, as the
/// payload's own tag wherever that already names it.
/// The match the resolver runs binds its last variant with a `_` arm, sound because the support
/// `dispatch` hands back only positions inside `VARIANTS`.
fn carried(
    contract: &TypeContract,
    tag: &str,
    variants: &[crate::contract::TaggedVariant],
    routes: TagRoutes,
    fallback: &RustIdent,
    serialize_only: bool,
) -> TokenStream {
    let name = implementation_type(contract);
    let support = Scope::of(contract).support();
    let fallback_name = fallback;
    let fallback = ident(fallback);
    let table = TagTable::of(variants, routes);
    let wire_names = &table.names;
    let (absent, null) = (table.absent(), table.null());

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
    let resolver =
        if let [only] = variants {
            let body = read(only);
            quote! { |_, replay| #body }
        } else {
            let arms = variants.iter().zip(&table.patterns).enumerate().map(
                |(index, (variant, pattern))| {
                    let body = read(variant);
                    if index + 1 == variants.len() {
                        quote! { _ => #body, }
                    } else {
                        // Every position of a spelling of the variant's tag
                        quote! { #pattern => #body, }
                    }
                },
            );
            quote! {
                |variant, replay| match variant {
                    #(#arms)*
                }
            }
        };
    let write = carried_writes(contract, tag, variants, routes, &support);
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
                    #support::dispatch(TAG, VARIANTS, #absent, #null, payload, #resolver, #unknown)
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

/// The `Deserialize` of a keyed enum: a listed string or a single-member object with a listed key
/// picks its variant, and anything else — or a named payload its type cannot read — is kept
/// whole in the open arm.
///
/// Each resolver binds its last position with a `_` arm, sound because the support `keyed` hands
/// back only positions inside `UNITS` and `KEYS`.
fn keyed(
    contract: &TypeContract,
    variants: &[crate::contract::KeyedVariant],
    fallback: &RustIdent,
) -> TokenStream {
    let name = implementation_type(contract);
    let support = Scope::of(contract).support();
    let construct = |value: TokenStream| {
        if contract.docs().deprecated {
            quote! {
                {
                    #[expect(
                        deprecated,
                        reason = "the generated deserializer must construct this deprecated variant"
                    )]
                    let resolved = #value;
                    resolved
                }
            }
        } else {
            value
        }
    };
    // The support helper names a variant by its position among the unit or the data variants.
    let (units, data): (Vec<_>, Vec<_>) = variants
        .iter()
        .partition(|variant| variant.payload.is_none());
    let unit_names = units.iter().map(|variant| variant.wire_name.as_str());
    let key_names = data.iter().map(|variant| variant.wire_name.as_str());

    // Unit variants: a position becomes its variant.
    // A union with none answers `None`, which the helper never asks it for.
    let unit_values: Vec<TokenStream> = units
        .iter()
        .map(|variant| {
            let member = ident(&variant.rust_name);
            construct(quote! { Self::#member })
        })
        .collect();
    let unit = match unit_values.as_slice() {
        [] => quote! { |_| None },
        [only] => quote! { |_| Some(#only) },
        _ => {
            let arms = positions(unit_values);
            quote! { |unit| Some(match unit { #arms }) }
        }
    };

    // Data variants: a position reads its payload type out of the member's value.
    let read = |variant: &&crate::contract::KeyedVariant| {
        let member = ident(&variant.rust_name);
        let boxed = variant
            .payload
            .as_ref()
            .is_some_and(super::types::enum_type_is_boxed);
        // An unboxed variant is its own constructor, and a closure around it is a lint in the
        // consumer's build.
        if !boxed && !contract.docs().deprecated {
            return quote! { serde::Deserialize::deserialize(payload).map(Self::#member) };
        }
        let value = if boxed {
            quote! { Box::new(value) }
        } else {
            quote! { value }
        };
        let resolved = construct(quote! { Self::#member(#value) });
        quote! { serde::Deserialize::deserialize(payload).map(|value| #resolved) }
    };
    let reads: Vec<TokenStream> = data.iter().map(read).collect();
    let data_resolver = if let [only] = reads.as_slice() {
        quote! { |_, payload| #only }
    } else {
        let arms = positions(reads);
        quote! { |data, payload| match data { #arms } }
    };

    // The open arm, for everything else and for a payload its type refuses.
    let fallback = ident(fallback);
    let open = construct(quote! { Self::#fallback(Box::new(value)) });

    quote! {
        impl<'de> serde::Deserialize<'de> for #name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                const UNITS: &[&str] = &[#(#unit_names),*];
                const KEYS: &[&str] = &[#(#key_names),*];
                let payload: #support::Content<'de> =
                    serde::Deserialize::deserialize(deserializer)?;
                #support::keyed(payload, UNITS, KEYS, #unit, #data_resolver, |value| #open)
            }
        }
    }
}

/// Match arms over the positions `0..bodies.len()`, with the last one bound by `_`.
///
/// The support helpers only ever hand back positions inside their lists, so the last arm can take
/// the rest and the match needs no unreachable arm of its own.
fn positions(bodies: Vec<TokenStream>) -> TokenStream {
    let last = bodies.len().saturating_sub(1);
    let arms = bodies.into_iter().enumerate().map(|(index, body)| {
        if index == last {
            quote! { _ => #body, }
        } else {
            quote! { #index => #body, }
        }
    });
    quote! { #(#arms)* }
}

/// One `Serialize` arm per named variant of a carried-tag union.
fn carried_writes<'a>(
    contract: &'a TypeContract,
    tag: &'a str,
    variants: &'a [crate::contract::TaggedVariant],
    routes: TagRoutes,
    support: &'a TokenStream,
) -> impl Iterator<Item = TokenStream> + 'a {
    variants.iter().enumerate().map(move |(index, variant)| {
        let member = ident(&variant.rust_name);
        let value = variant.tag_value.as_str();
        let absent = routes.absent == Some(index);
        let null = routes.null == Some(index);
        // Through the support injector rather than a bare delegation: the payload's own tag
        // member may be unset or stale in a hand-built value, and the wire must name the arm
        // the caller chose.
        let write = if variant.aliases.is_empty() && !absent && !null {
            // One spelling: the tag is written as it, whatever the payload holds.
            quote! {
                Self::#member(value) => {
                    #support::serialize_carried(value, #tag, #value, serializer)
                }
            }
        } else {
            // Several ways to name the arm: the payload's own one is kept where it names this
            // arm, because rewriting an alias or filling in an absent tag changes what was sent.
            let names = variant.spellings();
            quote! {
                Self::#member(value) => {
                    static SPELLINGS: #support::Spellings = #support::Spellings {
                        names: &[#(#names),*],
                        absent: #absent,
                        null: #null,
                    };
                    #support::serialize_carried_as(value, #tag, &SPELLINGS, serializer)
                }
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

/// The tag spellings of a carried-tag union, laid out the way its decoders look them up.
pub(super) struct TagTable<'a> {
    /// Every spelling of every variant, each variant's written one first.
    pub(super) names: Vec<&'a str>,
    /// The pattern matching the positions in `names` of each variant's spellings.
    ///
    /// A variant's spellings sit together, so the pattern is one position or one range, which
    /// is also what Clippy asks of the consumer's build.
    pub(super) patterns: Vec<TokenStream>,
    /// The position of each variant's first spelling.
    starts: Vec<usize>,
    routes: TagRoutes,
}

impl<'a> TagTable<'a> {
    pub(super) fn of(variants: &'a [crate::contract::TaggedVariant], routes: TagRoutes) -> Self {
        let mut names = Vec::new();
        let mut patterns = Vec::new();
        let mut starts = Vec::new();
        for variant in variants {
            let start = names.len();
            names.extend(variant.spellings());
            let last = names.len().saturating_sub(1);
            patterns.push(if last > start {
                quote! { #start..=#last }
            } else {
                quote! { #start }
            });
            starts.push(start);
        }
        Self {
            names,
            patterns,
            starts,
            routes,
        }
    }

    /// The position a missing tag member resolves to, as an `Option<usize>` expression.
    pub(super) fn absent(&self) -> TokenStream {
        self.route(self.routes.absent)
    }

    /// The position a `null` tag member resolves to, as an `Option<usize>` expression.
    pub(super) fn null(&self) -> TokenStream {
        self.route(self.routes.null)
    }

    /// A variant's first position, which its decoder arm matches like any of its spellings.
    fn route(&self, variant: Option<usize>) -> TokenStream {
        variant
            .and_then(|variant| self.starts.get(variant))
            .map_or_else(|| quote! { None }, |position| quote! { Some(#position) })
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
