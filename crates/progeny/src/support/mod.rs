//! Static source shipped into generated crates.
//!
//! Emitted from a versioned file in the progeny tree rather than token-generated, because it does
//! not vary per document: generating it would be complexity with no input. The same file is compiled
//! as part of progeny, so it is type-checked and unit-tested here before it is shipped anywhere.
//!
//! In a generated crate it is `#[doc(hidden)]` and never part of that crate's public API.

mod buffered;
mod degradations;
mod lenient;
mod multipart;
#[cfg(test)]
mod presence;
mod serve;
mod upload;
// The names `multipart.rs` and `lenient.rs` reach with `super::`: in a generated crate the
// buffered and upload files are spliced flat into the support root, so these imports make
// progeny's own module tree answer the same paths.
use buffered::{Content, ContentDeserializer, choice, sharing};
use upload::{base64_decode, base64_encode};
// What the client runtime reaches as `super::…`; here only the `cfg(test)` copy of that
// runtime does, so the imports are gated the same way.
#[cfg(test)]
use degradations::{Decoded, DegradationKind, Degradations, Site};
#[cfg(test)]
use lenient::Lenient;
// Crate-visible for one reason: the bridge test beside the client renderer holds this file's
// `Style` and the API layer's to the same variant list through an exhaustive match, which a
// private module would put out of the test's reach.
pub(crate) mod style;

// The files that name an HTTP crate, compiled under `cfg(test)` against dev-dependencies so
// that the property this module is arranged around — the file progeny tests is the file it ships —
// holds for every shipped file. Before this they were type-checked only by the
// corpus compile gate: minutes-later feedback, on the two files most coupled to external crates.
// `dead_code` is expected rather than fixed because nothing in progeny calls into them; existing is
// their whole job here.
#[cfg(test)]
#[expect(
    dead_code,
    reason = "compiled to be type-checked and linted, not called"
)]
mod http;
#[cfg(test)]
#[expect(
    dead_code,
    reason = "compiled to be type-checked and linted, not called"
)]
mod router;
// It names `time`, which progeny depends on only to compile and test this file.
#[cfg(test)]
mod rfc3339;
// Its tests call into it, but sending a request needs a server they do not have.
#[cfg(test)]
#[expect(
    dead_code,
    reason = "compiled to be type-checked and linted; `send` needs a server to be called"
)]
mod credentials;

/// The buffering machinery the hand-written `Deserialize` implementations call into.
const BUFFERED: &str = include_str!("buffered.rs");

/// The degradation report: what a lenient decode tolerated, by place in the description.
const DEGRADATIONS: &str = include_str!("degradations.rs");
/// The lenient decoder.
const LENIENT: &str = include_str!("lenient.rs");

/// Three-state optional and nullable property presence.
const PRESENCE: &str = include_str!("presence.rs");

/// The file-part payload type multipart bodies hold.
const UPLOAD: &str = include_str!("upload.rs");

/// The RFC 3339 wrapper the `time` crate's date and time types are held in.
const RFC3339: &str = include_str!("rfc3339.rs");

/// Parameter serialization, one function per style row.
const STYLE: &str = include_str!("style.rs");

/// `multipart/form-data` body assembly.
const MULTIPART: &str = include_str!("multipart.rs");

/// Extraction rules and the rejection envelope, which have no HTTP crate in them.
const SERVE: &str = include_str!("serve.rs");

/// The client runtime: `Error`, `ResponseValue`, and the body decoders.
///
/// Names `reqwest`, so it compiles here under `cfg(test)` against a dev-dependency — see the module
/// declarations above — and again in every generated crate the corpus compile gate checks.
const HTTP: &str = include_str!("http.rs");

/// The serving runtime, compiled the same two ways: it names `axum`.
const ROUTER: &str = include_str!("router.rs");

/// The held credentials of a client whose description declares a scheme progeny sends, compiled
/// the same two ways as the client runtime it joins.
const CREDENTIALS: &str = include_str!("credentials.rs");

/// The parts of the shipped client runtime a client carries only where its description reaches
/// them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClientUse {
    /// Any multipart request body, which ships the multipart writer.
    pub(crate) multipart: bool,
    /// Any security scheme progeny sends, which ships the held credentials.
    pub(crate) credentials: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresenceUse {
    Omitted,
    Preserved,
}

/// Whether a client holds credentials, which ships `credentials.rs` beside the client runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialUse {
    None,
    Held,
}

impl From<bool> for CredentialUse {
    fn from(held: bool) -> Self {
        if held { Self::Held } else { Self::None }
    }
}

impl From<bool> for PresenceUse {
    fn from(preserved: bool) -> Self {
        if preserved {
            Self::Preserved
        } else {
            Self::Omitted
        }
    }
}

/// The support source as tokens, so it composes with the rendered items.
///
/// Parsed rather than pasted so that a syntax error in the shipped source is a progeny test failure
/// rather than a consumer's compile error. A file that does not parse falls back to being emitted
/// verbatim, which is what a caller would want anyway — but the module's own tests compile it, so
/// that cannot happen unnoticed.
pub(crate) fn tokens(
    client: bool,
    server: bool,
    gated: bool,
    body_limit: crate::config::BodyLimit,
    presence: PresenceUse,
    decoding: Decoding,
    credentials: CredentialUse,
) -> proc_macro2::TokenStream {
    let buffered = source(BUFFERED);
    // The report travels with every client response, however the response was decoded; the
    // decoder itself is only compiled where responses are read leniently.
    let degradations =
        (client || decoding.is_lenient()).then(|| degradations_tokens(decoding.is_lenient()));
    let lenient = decoding.lenient().map(lenient_tokens);
    let presence = (presence == PresenceUse::Preserved).then(|| presence_tokens(decoding));
    let upload = source_items(UPLOAD);
    let edges = edge_tokens(
        Edge {
            client,
            server,
            gated,
            precise: false,
        },
        body_limit,
        ClientUse {
            credentials: credentials == CredentialUse::Held,
            ..ClientUse::default()
        },
        decoding,
    );
    let allow_dead_code = allow_dead_code();
    quote::quote! {
        #allow_dead_code
        #buffered
        #degradations
        #lenient
        #presence
        #upload
        #edges
    }
}

/// Support owned by a generated types crate.
pub(crate) fn types_tokens(presence: bool, decoding: Decoding) -> proc_macro2::TokenStream {
    let buffered = source(BUFFERED);
    let degradations = decoding.is_lenient().then(|| degradations_tokens(true));
    let lenient = decoding.lenient().map(lenient_tokens);
    let presence = presence.then(|| presence_tokens(decoding));
    let upload = source_items(UPLOAD);
    let allow_dead_code = allow_dead_code();
    quote::quote! {
        #allow_dead_code
        #buffered
        #degradations
        #lenient
        #presence
        #upload
    }
}

/// The inner attribute every generated support module opens with.
///
/// The runtime is shared by every description, and which parts of it one description reaches
/// varies.
/// Whether the consumer's toolchain calls the rest dead varies too: Rust 1.88 treats an item that
/// carries `#[expect(dead_code)]` as live, and so everything it calls, where later releases do
/// not.
/// No expectation progeny places is fulfilled under both, and an unfulfilled expectation is itself
/// a warning in the consumer's build, so this is an `allow`.
/// It is inner rather than outer because `clippy::allow_attributes` lints only the outer form, and
/// a consumer denying that lint must still be able to build the module.
fn allow_dead_code() -> proc_macro2::TokenStream {
    quote::quote! {
        #![allow(
            dead_code,
            reason = "the support runtime is shared by every description; which parts one uses varies, and so does whether the toolchain calls the rest dead"
        )]
    }
}

/// The RFC 3339 wrapper, for the support module of the crate whose types hold `time` values.
pub(crate) fn rfc3339_tokens() -> proc_macro2::TokenStream {
    source_items(RFC3339)
}

/// How the support module reads responses: strictly, or leniently against the given leaves.
///
/// The configuration's choice, joined with the one fact the lenient decoder needs from the
/// rest of the generator — which leaf types beyond `std` it has to be able to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decoding {
    Strict,
    Lenient(LeafUse),
}

impl Decoding {
    pub(crate) fn is_lenient(self) -> bool {
        matches!(self, Self::Lenient(_))
    }

    /// The leaves the lenient decoder implements itself for, when responses are read leniently.
    pub(crate) fn lenient(self) -> Option<LeafUse> {
        match self {
            Self::Strict => None,
            Self::Lenient(leaves) => Some(leaves),
        }
    }
}

/// The report vocabulary as a submodule, re-exported at the support root — where the client
/// runtime reaches it as `super::…` and the types module as `super::support::…`.
fn degradations_tokens(lenient: bool) -> proc_macro2::TokenStream {
    let degradations = degradations_module(lenient);
    quote::quote! {
        #degradations
        pub use degradations::{
            Decoded, Degradation, DegradationKind, Degradations, MemberSite, Site,
        };
    }
}

/// The report as a submodule, without the re-exports a caller names it by.
///
/// Shipped without the lenient decoder, it leaves out the methods only that decoder calls, which
/// would otherwise be dead code in the consumer's build.
fn degradations_module(lenient: bool) -> proc_macro2::TokenStream {
    let Some(mut file) = shipped(DEGRADATIONS) else {
        return DEGRADATIONS.parse().unwrap_or_default();
    };
    if !lenient {
        drop_methods(&mut file, "Degradations", &["merge_tolerated"]);
    }
    let degradations = quote::quote! { #file };
    quote::quote! {
        pub mod degradations {
            #degradations
        }
    }
}

/// The leaf types the lenient decoder has to read that the shipped file does not name itself:
/// the format crates a configuration chose, and the map types other than its `BTreeMap`.
///
/// `HashMap` is in `std`, and still left out of the shipped file unless the configuration chose
/// it: a consumer may refuse it for its iteration order, the way Clippy's `disallowed-types` can.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LeafUse {
    pub(crate) chrono: bool,
    pub(crate) time: bool,
    pub(crate) jiff: bool,
    pub(crate) uuid: bool,
    pub(crate) indexmap: bool,
    pub(crate) hashmap: bool,
}

impl LeafUse {
    /// Every leaf either names.
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            chrono: self.chrono || other.chrono,
            time: self.time || other.time,
            jiff: self.jiff || other.jiff,
            uuid: self.uuid || other.uuid,
            indexmap: self.indexmap || other.indexmap,
            hashmap: self.hashmap || other.hashmap,
        }
    }
}

/// The lenient decoder as a submodule, with the leaf impls the configuration needs and its
/// vocabulary re-exported at the support root beside the report's.
fn lenient_tokens(leaves: LeafUse) -> proc_macro2::TokenStream {
    let lenient = source(LENIENT);
    let mut leaf_types: Vec<proc_macro2::TokenStream> = Vec::new();
    if leaves.chrono {
        leaf_types.push(quote::quote! {
            chrono::DateTime<chrono::Utc>, chrono::NaiveDate, chrono::NaiveTime
        });
    }
    if leaves.time {
        leaf_types.push(quote::quote! {
            super::Rfc3339<time::OffsetDateTime>,
            super::Rfc3339<time::Date>,
            super::Rfc3339<time::Time>
        });
    }
    if leaves.jiff {
        leaf_types.push(quote::quote! { jiff::Timestamp, jiff::civil::Date, jiff::civil::Time });
    }
    if leaves.uuid {
        leaf_types.push(quote::quote! { uuid::Uuid });
    }
    let extra_leaves = (!leaf_types.is_empty()).then(|| {
        quote::quote! { lenient_via_serde! { #(#leaf_types),* } }
    });
    let mut map_types = Vec::new();
    if leaves.indexmap {
        map_types.push(quote::quote! { indexmap::IndexMap });
    }
    if leaves.hashmap {
        map_types.push(quote::quote! { std::collections::HashMap });
    }
    let maps = map_types.iter().map(|map| {
        quote::quote! {
            impl<'de, T> Lenient<'de> for #map<String, T>
            where
                T: Lenient<'de>,
            {
                const ROOT: Site = T::ROOT;

                fn lenient(
                    content: Content<'de>,
                    site: Site,
                    report: &mut Degradations,
                ) -> Result<Self, Problem> {
                    entries(content, site, report).map(|pairs| pairs.into_iter().collect())
                }
            }
        }
    });
    // `Lenient` and `Problem` are the decoder's public face — the trait is the bound on
    // `Decoded::from_json` and names `Problem` in its method signature, and under workspace
    // packaging the client crate reaches both across the crate wall — so they are re-exported
    // at the support root beside the report's vocabulary. The rest is `pub(crate)` machinery a
    // generated impl reaches through `lenient::`, the way it reaches `style::` and `multipart::`:
    // re-exporting it here would be an import a document that has no union leaves unused.
    quote::quote! {
        pub mod lenient {
            #lenient
            #extra_leaves
            #(#maps)*
        }
        pub use lenient::{Lenient, Problem};
    }
}

fn presence_tokens(decoding: Decoding) -> proc_macro2::TokenStream {
    let presence = source_items(PRESENCE);
    let lenient = decoding.is_lenient().then(presence_lenient_tokens);
    quote::quote! {
        #presence
        #lenient

        /// Reads a presence-preserving field, defaulting only when the member is absent.
        pub(crate) fn take_presence_or_default<'de, T, E>(
            buffer: &mut Buffer<'de>,
            name: &'static str,
        ) -> Result<T, E>
        where
            T: serde::Deserialize<'de> + Default,
            E: serde::de::Error,
        {
            match buffer.position(name) {
                Some(content) => serde::Deserialize::deserialize(
                    ContentDeserializer::new(content),
                ),
                None => Ok(T::default()),
            }
        }
    }
}

/// The presence-preserving field's half of the lenient decoder: the impl, and the member reader
/// a struct's decoder calls for one.
fn presence_lenient_tokens() -> proc_macro2::TokenStream {
    quote::quote! {


        impl<'de, T> Lenient<'de> for Presence<T>
        where
            T: Lenient<'de>,
        {
            const ROOT: Site = T::ROOT;

            /// `null` is `Null`; the caller decides what absence is, because only it saw the
            /// member missing.
            fn lenient(
                content: Content<'de>,
                site: Site,
                report: &mut Degradations,
            ) -> Result<Self, lenient::Problem> {
                match content {
                    Content::None | Content::Unit => Ok(Presence::Null),
                    Content::Some(inner) => T::lenient(*inner, site, report).map(Presence::Value),
                    other => T::lenient(other, site, report).map(Presence::Value),
                }
            }
        }

        impl<'de> lenient::Members<'de> {
            /// Read a presence-preserving member: absent is `Omitted`, `null` is `Null`, and a
            /// value that does not read as `T` is `Omitted` with the reason recorded.
            pub(crate) fn take_presence<T>(
                &mut self,
                member: MemberSite,
                report: &mut Degradations,
            ) -> Presence<T>
            where
                T: Lenient<'de>,
            {
                let site = member.site();
                match self.take_raw(member, report) {
                    lenient::Slot::Absent => Presence::Omitted,
                    lenient::Slot::Null => Presence::Null,
                    lenient::Slot::Value(content) => match T::lenient(content, site, report) {
                        Ok(value) => Presence::Value(value),
                        Err(err) => {
                            report.record(
                                site,
                                DegradationKind::Undecodable,
                                Some(err.to_string()),
                                true,
                            );
                            Presence::Omitted
                        }
                    },
                }
            }
        }
    }
}

/// Support owned by a generated client crate.
///
/// Decoded leniently, the decoding vocabulary is the types crate's — one `Degradations` type,
/// shared by the `Lenient` impls there and the client runtime here — so it is re-exported from
/// the crate root's `types`, which is where the client runtime's `super::…` paths look.
/// Decoded strictly, the client carries a report of its own.
pub(crate) fn client_tokens(used: ClientUse, decoding: Decoding) -> proc_macro2::TokenStream {
    // Shipped only where the multipart writer's `super::base64_decode` needs resolving; with no
    // multipart body in the description nothing here would be live at all.
    let upload = used.multipart.then(|| source_items(UPLOAD));
    let edges = edge_tokens(
        Edge::CLIENT,
        crate::config::BodyLimit::default(),
        used,
        decoding,
    );
    // Lenient: the report is the types crate's, because the `Lenient` impls there record into
    // it.
    // Strict: nothing in the types crate names a report, so the client carries its own.
    // Its support module is private, so only what the runtime names is re-exported: anything
    // more would be an unused import in the consumer's build.
    let vocabulary = if decoding.is_lenient() {
        quote::quote! {
            pub use super::types::{Decoded, DegradationKind, Degradations, Lenient, Site};
        }
    } else {
        let degradations = degradations_module(false);
        quote::quote! {
            #degradations
            pub use degradations::{Decoded, Degradations};
        }
    };
    let allow_dead_code = allow_dead_code();
    quote::quote! {
        #allow_dead_code
        #vocabulary
        #upload
        #edges
    }
}

/// Support owned by a generated server crate.
///
/// The upload source is spliced in only so the multipart submodule's `super::` calls resolve: the
/// type a server parses into lives in the types it imports.
pub(crate) fn server_tokens(body_limit: crate::config::BodyLimit) -> proc_macro2::TokenStream {
    let upload = source_items(UPLOAD);
    let edges = edge_tokens(
        Edge::SERVER,
        body_limit,
        ClientUse::default(),
        Decoding::Strict,
    );
    let allow_dead_code = allow_dead_code();
    quote::quote! {
        #allow_dead_code
        #upload
        #edges
    }
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    client: bool,
    server: bool,
    gated: bool,
    /// Whether the edge is a workspace crate of its own, whose `ClientUse` says what it reaches.
    precise: bool,
}

impl Edge {
    const CLIENT: Self = Self {
        client: true,
        server: false,
        gated: false,
        precise: true,
    };
    const SERVER: Self = Self {
        client: false,
        server: true,
        gated: false,
        precise: true,
    };
}

fn edge_tokens(
    edge: Edge,
    body_limit: crate::config::BodyLimit,
    client_use: ClientUse,
    decoding: Decoding,
) -> proc_macro2::TokenStream {
    let Edge {
        client,
        server,
        gated,
        precise,
    } = edge;
    if !client && !server {
        return proc_macro2::TokenStream::new();
    }
    // The shipped tree keeps **progeny's own module layout**: `style` and `multipart` are submodules
    // here and submodules there, so a path that resolves while progeny compiles resolves in the
    // consumer's crate too. Flattening them was a standing chance for the two to disagree, and the
    // disagreement would surface as a compile error in somebody else's build.
    let style = source(STYLE);
    // A client crate whose description sends no multipart body leaves the writer out.
    let multipart = (!precise || !client || client_use.multipart).then(|| source(MULTIPART));
    let multipart = multipart.map(|multipart| quote::quote! { pub mod multipart { #multipart } });

    // Gated only in crate mode. A module tree is `include!`d into somebody else's crate, which has
    // no `client` feature to speak of and never asked for one; the caller opted in by configuring
    // the halves, and that is the whole of the decision there.
    let calling = (client && gated).then(|| quote::quote! { #[cfg(feature = "client")] });
    let serving = (server && gated).then(|| quote::quote! { #[cfg(feature = "server")] });
    // Nested behind their feature and re-exported, so callers still write `support::Error`. A
    // consumer who turned a half off must not pay for its HTTP stack, and these are the only shipped
    // items that name one — putting each in its own module is what lets one attribute cover all of
    // them rather than one per item.
    let wire = client.then(|| {
        let wire = client_source(HTTP, decoding);
        // Only a description with a scheme progeny sends holds credentials, so only its client
        // pays for them, under either packaging.
        // Spliced flat into the client runtime's module, so its items arrive without the file's
        // own attributes.
        let credentials = client_use.credentials.then(|| source_items(CREDENTIALS));
        quote::quote! {
            #calling
            pub mod wire {
                #wire
                #credentials
            }
            #calling
            pub use wire::*;
        }
    });
    let serve = server.then(|| {
        let serve = source(SERVE);
        let router = with_body_limit(ROUTER, body_limit);
        quote::quote! {
            #serving
            pub mod serve { #serve }
            #serving
            pub mod router { #router }
            #serving
            pub use serve::*;
            #serving
            pub use router::*;
        }
    });

    quote::quote! {
        pub mod style { #style }

        #multipart

        #wire
        #serve
    }
}

/// Replace `BODY_LIMIT`'s value with the configured one.
///
/// The constant is rewritten rather than templated in, so the shipped file stays a file that
/// compiles and is unit-tested here with its own default in place. The alternative — a placeholder
/// only meaningful after substitution — would mean the source progeny tests is not the source it
/// ships, which is the property this whole module is arranged around.
///
/// **This mechanism is for one knob, and deliberately so.** Each constant it rewrites needs its own
/// surgery here and its own test pinning that the configured value arrives, so it scales by
/// hand-work. The day a *second* shipped constant becomes configuration, replace it — a generated
/// `support::consts` module the shipped files `use`, one mechanism for every knob — rather than
/// adding a second rewrite beside this one. Building that module today, for one knob, would be the
/// speculative generality this project refuses everywhere else.
fn with_body_limit(text: &str, limit: crate::config::BodyLimit) -> proc_macro2::TokenStream {
    let Some(mut file) = shipped(text) else {
        return text.parse().unwrap_or_default();
    };
    let bytes = limit.0;
    for item in &mut file.items {
        if let syn::Item::Const(konst) = item
            && konst.ident == "BODY_LIMIT"
        {
            *konst.expr = syn::parse_quote! { #bytes };
        }
    }
    quote::quote! { #file }
}

/// One shipped file as it goes into a generated crate.
///
/// The tests that verify it here are left out: they belong beside the code they test and have no
/// business in a consumer's crate, where they would be compile time nobody asked for, in a module
/// a consumer cannot even read the source of.
/// So is the file-wide `dead_code` expectation progeny's own build needs, where the file is
/// compiled but mostly not called: in a generated crate the support module's [`allow_dead_code`]
/// decides instead.
fn shipped(text: &str) -> Option<syn::File> {
    let mut file = syn::parse_file(text).ok()?;
    file.items.retain(|item| !is_test_module(item));
    file.attrs.retain(|attribute| {
        let suppression =
            attribute.path().is_ident("expect") || attribute.path().is_ident("cfg_attr");
        !(suppression
            && quote::quote! { #attribute }
                .to_string()
                .contains("dead_code"))
    });
    Some(file)
}

/// One shipped file as tokens, with its own inner attributes.
fn source(text: &str) -> proc_macro2::TokenStream {
    let Some(file) = shipped(text) else {
        return text.parse().unwrap_or_default();
    };
    quote::quote! { #file }
}

/// Leave the named methods out of a shipped file's inherent `impl` blocks for `ty`.
fn drop_methods(file: &mut syn::File, ty: &str, names: &[&str]) {
    for item in &mut file.items {
        if let syn::Item::Impl(implementation) = item
            && implementation.trait_.is_none()
            && impl_type_is(implementation, ty)
        {
            implementation.items.retain(|member| {
                !matches!(member, syn::ImplItem::Fn(method)
                    if names.iter().any(|name| method.sig.ident == name))
            });
        }
    }
}

/// The client runtime, with the lenient decoders left out when nothing is decoded leniently —
/// they name a trait that only exists then.
/// Decoded leniently, the strict header decoder goes instead, because no arm calls it.
fn client_source(text: &str, decoding: Decoding) -> proc_macro2::TokenStream {
    let Some(mut file) = shipped(text) else {
        return text.parse().unwrap_or_default();
    };
    // The report and its vocabulary stay: `ResponseValue` carries a report whichever way its
    // body was decoded.
    // Only the lenient decoders go — the body's two functions and the header's one, the one
    // `use` that names what only they name, which `http.rs` keeps on a line of its own for
    // exactly this, and below, the two methods only they and their arms call.
    let lenient_decoder = |item: &syn::Item| match item {
        syn::Item::Fn(function) => {
            let name = function.sig.ident.to_string();
            name.starts_with("decode_lenient") || name == "lenient_header"
        }
        syn::Item::Use(import) => {
            let spelled = quote::quote! { #import }.to_string();
            spelled == quote::quote! { use super::{DegradationKind, Lenient, Site}; }.to_string()
        }
        _ => false,
    };
    let strict_decoder = |item: &syn::Item| matches!(item, syn::Item::Fn(function) if function.sig.ident == "strict_header");
    file.items.retain(|item| {
        if decoding.is_lenient() {
            !strict_decoder(item)
        } else {
            !lenient_decoder(item)
        }
    });
    // The constructor that takes a decoded report, and the header reader that records into one
    if !decoding.is_lenient() {
        drop_methods(&mut file, "ResponseValue", &["decoded", "read_headers"]);
    }
    quote::quote! { #file }
}

fn impl_type_is(implementation: &syn::ItemImpl, expected: &str) -> bool {
    let syn::Type::Path(path) = implementation.self_ty.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

/// One shipped file as its items alone, without the file's own inner attributes.
fn source_items(text: &str) -> proc_macro2::TokenStream {
    let Some(file) = shipped(text) else {
        return text.parse().unwrap_or_default();
    };
    let items = file.items;
    quote::quote! { #(#items)* }
}

/// The same, as items alone.
///
/// A file's own `#![doc]` and `#![expect]` are *inner* attributes, and inner attributes may only
/// open a block. Two files nested into one module would put the second file's inner attributes
/// after the first file's items, which is not Rust — and the failure is quiet, because the renderer
/// falls back to emitting unparsed tokens rather than failing a build.
#[cfg(test)]
fn items(text: &str) -> Vec<syn::Item> {
    let Ok(file) = syn::parse_file(text) else {
        return Vec::new();
    };
    file.items
        .into_iter()
        .filter(|item| !is_test_module(item))
        .collect()
}

/// Whether an item is a `#[cfg(test)] mod`, which the shipped source drops.
///
/// The question is asked structurally — the `cfg` predicate is exactly the path `test` — rather
/// than as a substring of the stringified attribute, which would also strip a shipped
/// `#[cfg(not(test))]` or `#[cfg(feature = "latest")]` module, silently.
fn is_test_module(item: &syn::Item) -> bool {
    let syn::Item::Mod(module) = item else {
        return false;
    };
    module.attrs.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Path>()
                .is_ok_and(|predicate| predicate.is_ident("test"))
    })
}

#[cfg(test)]
mod tests {
    use crate::config::BodyLimit;
    use color_eyre::eyre::{self, OptionExt as _};
    use serde::de::{Deserialize, Deserializer};

    /// The one flattened file names no `super::` paths.
    ///
    /// `buffered.rs` is `mod buffered;` in this workspace and spliced *flat into the support
    /// root* in a generated crate — the one file whose module depth differs between the two
    /// forms. A `super::` path inside it would resolve to two different places, compile here,
    /// and break every generated crate, invisibly to this workspace's build. (`style`,
    /// `multipart`, `serve` and the wire module keep their submodule depth, so `super::` is
    /// legitimate there.)
    #[test_util::test]
    fn the_flattened_buffered_source_names_no_super_paths() {
        for (name, text) in [
            ("buffered.rs", super::BUFFERED),
            ("upload.rs", super::UPLOAD),
        ] {
            let items = super::items(text);
            let spelled = quote::quote! { #(#items)* }.to_string();
            assert!(
                !spelled.contains("super ::"),
                "{name} names `super::`, which resolves differently once the file is spliced \
                 flat into the generated support root",
            );
        }
    }

    /// The two flattened files share one namespace, so a named import in the second is a
    /// collision waiting for the first to declare the same name — which `std::fmt` and
    /// `serde::de` already did. Only trait-anchoring `as _` imports bind no name.
    #[test_util::test]
    fn the_second_flattened_source_imports_no_names() {
        let Ok(file) = syn::parse_file(super::UPLOAD) else {
            panic!("upload.rs should parse");
        };
        for item in &file.items {
            if let syn::Item::Use(import) = item {
                let spelled = quote::quote! { #import }.to_string();
                assert!(
                    spelled.ends_with("as _ ;"),
                    "upload.rs imports `{spelled}`, which can collide with buffered.rs once both \
                     are spliced flat into the generated support root; qualify the path instead",
                );
            }
        }
    }

    /// The shipped source has to parse as a file, both ways.
    ///
    /// It is checked here because the renderer's fallback is to emit unparsed tokens rather than
    /// fail a build — the right call for a formatting failure, and the reason a *composition*
    /// failure would otherwise reach a consumer as one very long line that happens not to compile.
    #[test_util::test]
    fn the_shipped_module_parses_as_a_file() {
        for client in [false, true] {
            for server in [false, true] {
                for gated in [false, true] {
                    for credentials in [false, true] {
                        let tokens = super::tokens(
                            client,
                            server,
                            gated,
                            BodyLimit::default(),
                            false.into(),
                            super::Decoding::Strict,
                            credentials.into(),
                        );
                        syn::parse2::<syn::File>(tokens.clone()).map_err(|error| {
                            eyre::eyre!(
                                "{}",
                                indoc::formatdoc! {"
                                    support(client = {client}, server = {server}, gated = \
                                    {gated}, credentials = {credentials}) does not parse: {error}
                                    {tokens}"
                                }
                            )
                        })?;
                    }
                }
            }
        }
        // And each half is really there when it was asked for, so the check above is not passing
        // because there was nothing to compose.
        let both = super::tokens(
            true,
            true,
            true,
            BodyLimit::default(),
            false.into(),
            super::Decoding::Strict,
            false.into(),
        )
        .to_string();
        assert!(both.contains("ResponseValue"), "{both}");
        assert!(both.contains("Rejection"), "{both}");
        assert!(both.contains("cfg (feature = \"client\")"), "{both}");
        assert!(both.contains("cfg (feature = \"server\")"), "{both}");

        let calling = super::tokens(
            true,
            false,
            true,
            BodyLimit::default(),
            false.into(),
            super::Decoding::Strict,
            false.into(),
        )
        .to_string();
        assert!(calling.contains("ResponseValue"), "{calling}");
        assert!(!calling.contains("Rejection"), "{calling}");
        // Held credentials ship only to a client whose description has a scheme to hold.
        assert!(!calling.contains("Credentials"), "{calling}");
        let holding = super::tokens(
            true,
            false,
            true,
            BodyLimit::default(),
            false.into(),
            super::Decoding::Strict,
            true.into(),
        )
        .to_string();
        assert!(holding.contains("pub struct Credentials"), "{holding}");

        let serving = super::tokens(
            false,
            true,
            true,
            BodyLimit::default(),
            false.into(),
            super::Decoding::Strict,
            false.into(),
        )
        .to_string();
        assert!(!serving.contains("ResponseValue"), "{serving}");
        assert!(serving.contains("Rejection"), "{serving}");

        // Both halves need the style table and the multipart writer, so neither is behind a gate.
        assert!(calling.contains("query_pairs"), "{calling}");
        assert!(serving.contains("query_pairs"), "{serving}");

        // In module mode nothing is gated: there is no crate whose feature could turn it on.
        assert!(
            !super::tokens(
                true,
                true,
                false,
                BodyLimit::default(),
                false.into(),
                super::Decoding::Strict,
                false.into(),
            )
            .to_string()
            .contains("cfg (feature"),
        );
    }

    /// Each decoding ships the header decoder its arms call and leaves out the other, and a strict
    /// client leaves out the body decoder and the methods only the lenient decoders call.
    #[test_util::test]
    fn the_header_decoders_follow_the_decoding() {
        /// Whether each named item ships.
        fn shipped(decoding: super::Decoding, names: &[&str]) -> eyre::Result<Vec<bool>> {
            let file: syn::File = syn::parse2(super::client_source(super::HTTP, decoding))?;
            let find = |name: &str| {
                file.items.iter().any(|item| {
                    match item {
                    syn::Item::Fn(function) => function.sig.ident == name,
                    syn::Item::Struct(structure) => structure.ident == name,
                    syn::Item::Impl(implementation)
                        if super::impl_type_is(implementation, "ResponseValue") =>
                    {
                        implementation.items.iter().any(|member| {
                            matches!(member, syn::ImplItem::Fn(method) if method.sig.ident == name)
                        })
                    }
                    _ => false,
                }
                })
            };
            Ok(names.iter().map(|name| find(name)).collect())
        }

        let names = [
            "HeaderRow",
            "header_value",
            "lenient_header",
            "strict_header",
            "read_headers",
            "decode_lenient",
            "decoded",
        ];

        // Lenient: everything but the strict header reader
        assert_eq!(
            shipped(super::Decoding::Lenient(super::LeafUse::default()), &names)?,
            [true, true, true, false, true, true, true]
        );
        // Strict: the strict header reader, and nothing that records into a report
        assert_eq!(
            shipped(super::Decoding::Strict, &names)?,
            [true, true, false, true, false, false, false]
        );
    }

    /// Every support module opens with the one inner `allow(dead_code)` and carries no
    /// `dead_code` expectation, whichever entry point assembles it.
    ///
    /// Which parts of the runtime a description reaches, and whether the consumer's toolchain
    /// calls the rest dead, are both beyond progeny's knowledge, so an expectation anywhere in the
    /// module would be unfulfilled in somebody's build.
    #[test_util::test]
    fn every_support_module_allows_dead_code_and_expects_none() {
        let lenient = super::Decoding::Lenient(super::LeafUse {
            chrono: true,
            time: true,
            jiff: true,
            uuid: true,
            indexmap: true,
            hashmap: true,
        });
        let mut modules = Vec::new();
        for decoding in [super::Decoding::Strict, lenient] {
            for client in [false, true] {
                for server in [false, true] {
                    modules.push(super::tokens(
                        client,
                        server,
                        true,
                        BodyLimit::default(),
                        true.into(),
                        decoding,
                        client.into(),
                    ));
                }
            }
            modules.push(super::types_tokens(true, decoding));
            for multipart in [false, true] {
                for credentials in [false, true] {
                    modules.push(super::client_tokens(
                        super::ClientUse {
                            multipart,
                            credentials,
                        },
                        decoding,
                    ));
                }
            }
        }
        modules.push(super::server_tokens(BodyLimit::default()));

        for module in modules {
            let file: syn::File = syn::parse2(module)?;
            let suppressions: Vec<String> = file
                .attrs
                .iter()
                .filter(|attribute| !attribute.path().is_ident("doc"))
                .map(|attribute| quote::quote! { #attribute }.to_string())
                .collect();
            assert_eq!(suppressions.len(), 1, "{suppressions:?}");
            let allow: String = suppressions
                .first()
                .ok_or_eyre("the module carries a suppression")?
                .split_whitespace()
                .collect();
            assert!(allow.starts_with("#![allow(dead_code,reason="), "{allow}");
            // The allow is the only mention of the lint: no item or nested file expects it.
            let flat: String = quote::quote! { #file }
                .to_string()
                .split_whitespace()
                .collect();
            assert!(!flat.contains("expect(dead_code"), "{flat}");
            assert_eq!(flat.matches("dead_code").count(), 1, "{flat}");
        }
    }

    /// The body ceiling is a knob, and the shipped constant is what it turns.
    ///
    /// Carried in from stage 7, where it was a constant with a comment claiming `DefaultBodyLimit`
    /// could raise it — which it cannot: that layer inserts an extension only extractors calling
    /// `with_limited_body` consult, and this reads the body with `to_bytes` and its own number.
    /// Saying so in a comment was not enough, so it became configuration.
    #[test_util::test]
    fn the_body_ceiling_is_the_configured_one() {
        let rendered = super::tokens(
            false,
            true,
            false,
            BodyLimit(4096),
            false.into(),
            super::Decoding::Strict,
            false.into(),
        )
        .to_string();
        assert!(
            rendered.contains("BODY_LIMIT : usize = 4096"),
            "the configured ceiling did not reach the shipped constant"
        );
        // And the default is still the default rather than something a caller has to know to set.
        let fallback = super::tokens(
            false,
            true,
            false,
            BodyLimit::default(),
            false.into(),
            super::Decoding::Strict,
            false.into(),
        )
        .to_string();
        assert!(
            fallback.contains("BODY_LIMIT : usize = 2097152"),
            "{fallback}"
        );
    }

    use super::buffered::{
        Assemble, Buffer, BufferVisitor, Content, ContentDeserializer, Missing, Unknown,
    };

    /// The shape the spike hand-writes: a required member, an optional one, and one whose wire name
    /// is not its Rust name.
    #[derive(Debug, PartialEq)]
    struct Spike {
        required: String,
        optional: Option<i64>,
        renamed: bool,
    }

    /// Written by hand here in exactly the form the renderer emits, so that the machinery is tested
    /// through the same shape the generated code uses.
    impl<'de> Assemble<'de> for Spike {
        const NAME: &'static str = "Spike";
        const FIELDS: &'static [&'static str] = &["required", "optional", "wireName"];

        fn assemble<E>(buffer: &mut Buffer<'de>) -> Result<Self, E>
        where
            E: serde::de::Error,
        {
            Ok(Self {
                required: buffer.take("required")?,
                optional: buffer.take("optional")?,
                renamed: buffer.take("wireName")?,
            })
        }
    }

    impl<'de> Deserialize<'de> for Spike {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_struct(
                Self::NAME,
                Self::FIELDS,
                BufferVisitor::<Self>::new(Unknown::Ignore),
            )
        }
    }

    fn parse(text: &str) -> Result<Spike, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test_util::test]
    fn every_member_is_read_by_its_wire_name() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": 1, "wireName": true}"#)?,
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
    }

    #[test_util::test]
    fn a_bare_option_accepts_an_absent_member() {
        // No `default` attribute anywhere: serde routes a missing member through a deserializer
        // whose `deserialize_option` answers `visit_none`, and this mirrors that.
        assert_eq!(
            parse(r#"{"required": "a", "wireName": false}"#)?.optional,
            None
        );
    }

    #[test_util::test]
    fn an_explicit_null_and_an_absent_member_agree() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": null, "wireName": false}"#)?.optional,
            None
        );
    }

    #[test_util::test]
    fn a_duplicate_member_is_rejected_rather_than_last_write_wins() {
        // The case a map-based buffer would lose silently, which is why the buffer is a list.
        let error = parse(r#"{"required": "a", "required": "b", "wireName": false}"#)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(
            error.to_string().starts_with("duplicate field `required`"),
            "{error}"
        );
    }

    #[test_util::test]
    fn a_missing_member_says_which_one() {
        let error = parse(r#"{"wireName": false}"#)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(
            error.to_string().starts_with("missing field `required`"),
            "{error}"
        );
    }

    #[test_util::test]
    fn an_unknown_member_is_ignored_under_that_policy() {
        assert!(
            parse(r#"{"required": "a", "wireName": false, "surprise": {"deep": [1]}}"#).is_ok()
        );
    }

    #[test_util::test]
    fn an_unknown_member_is_refused_under_the_other_one() {
        let mut deserializer =
            serde_json::Deserializer::from_str(r#"{"required": "a", "surprise": 1}"#);
        let error = deserializer
            .deserialize_struct(
                <Spike as Assemble>::NAME,
                <Spike as Assemble>::FIELDS,
                BufferVisitor::<Spike>::new(Unknown::Deny),
            )
            .err()
            .ok_or_eyre("an undeclared member should be refused")?;
        assert!(
            error.to_string().starts_with("unknown field `surprise`"),
            "{error}"
        );
    }

    /// The derive's shape of the spike, read through [`super::buffered::Object`] the way the
    /// renderer emits it under the derive strategy.
    #[derive(Debug, PartialEq)]
    struct Derived {
        required: String,
        optional: Option<i64>,
    }

    impl<'de> Deserialize<'de> for Derived {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            #[derive(serde::Deserialize)]
            #[serde(remote = "Derived")]
            struct Members {
                required: String,
                optional: Option<i64>,
            }
            Members::deserialize(super::buffered::Object(deserializer))
        }
    }

    /// A struct reads an object and nothing else, under either strategy, with one message.
    #[test_util::test]
    fn a_struct_refuses_a_sequence_under_either_strategy() {
        let refused = |error: serde_json::Error| {
            assert!(
                error
                    .to_string()
                    .starts_with("invalid type: sequence, expected struct "),
                "{error}"
            );
        };
        refused(
            serde_json::from_str::<Spike>(r#"["a", 1, true]"#)
                .err()
                .ok_or_eyre("the hand-written struct should refuse a sequence")?,
        );
        refused(
            serde_json::from_str::<Derived>(r#"["a", 1]"#)
                .err()
                .ok_or_eyre("the derived struct should refuse a sequence")?,
        );
        assert_eq!(
            serde_json::from_str::<Derived>(r#"{"required": "a"}"#)?,
            Derived {
                required: "a".to_owned(),
                optional: None,
            }
        );
    }

    /// An untagged union replays its payload into each variant, and a struct variant before a
    /// list one leaves a list to it.
    #[test_util::test]
    fn a_struct_branch_leaves_a_list_to_a_later_list_branch() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        #[serde(untagged)]
        enum Hand {
            Spike(Spike),
            List(Vec<serde_json::Value>),
        }
        #[derive(Debug, PartialEq, serde::Deserialize)]
        #[serde(untagged)]
        enum Derive {
            Derived(Derived),
            List(Vec<String>),
        }
        // `["a", 1, true]` fills the spike's three members in order, which a positional read took.
        assert_eq!(
            serde_json::from_str::<Hand>(r#"["a", 1, true]"#)?,
            Hand::List(vec![
                serde_json::json!("a"),
                serde_json::json!(1),
                serde_json::json!(true)
            ])
        );
        assert_eq!(
            serde_json::from_str::<Derive>(r#"["a"]"#)?,
            Derive::List(vec!["a".to_owned()])
        );
        // The same one level down, where a list of structs meets a list of lists.
        assert_eq!(
            serde_json::from_str::<Vec<Derive>>(r#"[["a"], {"required": "b"}]"#)?,
            vec![
                Derive::List(vec!["a".to_owned()]),
                Derive::Derived(Derived {
                    required: "b".to_owned(),
                    optional: None,
                }),
            ]
        );
    }

    #[test_util::test]
    fn a_member_of_the_wrong_type_reports_it_the_way_the_format_does() {
        let error = parse(r#"{"required": 7, "wireName": false}"#)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(
            error
                .to_string()
                .starts_with("invalid type: integer `7`, expected a string"),
            "{error}"
        );
    }

    #[test_util::test]
    fn a_buffered_value_replays_into_any_type() {
        // The machinery has to be able to hand a buffered value to an arbitrary `Deserialize`, or
        // a struct could only hold scalars.
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2)]);
        let replayed: Vec<u64> =
            Vec::deserialize(ContentDeserializer::<serde_json::Error>::new(content))?;
        assert_eq!(replayed, [1, 2]);

        let content = Content::Map(vec![(
            Content::Str("a"),
            Content::Seq(vec![Content::Bool(true)]),
        )]);
        let replayed: std::collections::BTreeMap<String, Vec<bool>> =
            Deserialize::deserialize(ContentDeserializer::<serde_json::Error>::new(content))?;
        assert_eq!(replayed["a"], [true]);
    }

    #[test_util::test]
    fn a_missing_member_of_a_non_optional_type_is_an_error_and_not_a_default() {
        let outcome: Result<i64, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert!(outcome.is_err());
        let outcome: Result<Option<i64>, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert_eq!(outcome?, None);
    }
}
