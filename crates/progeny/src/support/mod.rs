//! Static source shipped into generated crates.
//!
//! Emitted from a versioned file in the progeny tree rather than token-generated, because it does
//! not vary per document: generating it would be complexity with no input. The same file is compiled
//! as part of progeny, so it is type-checked and unit-tested here before it is shipped anywhere.
//!
//! In a generated crate it is `#[doc(hidden)]` and never part of that crate's public API.

mod buffered;
mod multipart;
mod serve;
// Crate-visible for one reason: the bridge test beside the client renderer holds this file's
// `Style` and the API layer's to the same variant list through an exhaustive match, which a
// private module would put out of the test's reach.
pub(crate) mod style;

// The two files that name an HTTP crate, compiled under `cfg(test)` against dev-dependencies so
// that the property this module is arranged around — the file progeny tests is the file it ships —
// holds for all six shipped files rather than four. Before this they were type-checked only by the
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

/// The buffering machinery the hand-written `Deserialize` implementations call into.
const BUFFERED: &str = include_str!("buffered.rs");

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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ServerUse {
    pub(crate) json: bool,
    pub(crate) form: bool,
    pub(crate) multipart: bool,
    pub(crate) text: bool,
    pub(crate) bytes: bool,
    pub(crate) path: bool,
    pub(crate) query: bool,
    pub(crate) header: bool,
    pub(crate) cookie: bool,
    pub(crate) text_response: bool,
    pub(crate) byte_response: bool,
    pub(crate) styles: StyleUse,
    pub(crate) parts: PartUse,
}

impl ServerUse {
    const ALL: Self = Self {
        json: true,
        form: true,
        multipart: true,
        text: true,
        bytes: true,
        path: true,
        query: true,
        header: true,
        cookie: true,
        text_response: true,
        byte_response: true,
        styles: StyleUse::ALL,
        parts: PartUse::ALL,
    };
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClientUse {
    pub(crate) form: bool,
    pub(crate) multipart: bool,
    pub(crate) text_response: bool,
    pub(crate) byte_response: bool,
    pub(crate) query: bool,
    pub(crate) path: bool,
    pub(crate) matrix: bool,
    pub(crate) header: bool,
    pub(crate) cookie: bool,
    pub(crate) styles: StyleUse,
    pub(crate) parts: PartUse,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StyleUse {
    pub(crate) form: bool,
    pub(crate) simple: bool,
    pub(crate) label: bool,
    pub(crate) matrix: bool,
    pub(crate) space_delimited: bool,
    pub(crate) pipe_delimited: bool,
    pub(crate) deep_object: bool,
}

impl StyleUse {
    const ALL: Self = Self {
        form: true,
        simple: true,
        label: true,
        matrix: true,
        space_delimited: true,
        pipe_delimited: true,
        deep_object: true,
    };

    fn contains(self, variant: &syn::Ident) -> bool {
        match variant.to_string().as_str() {
            "Form" => self.form,
            "Simple" => self.simple,
            "Label" => self.label,
            "Matrix" => self.matrix,
            "SpaceDelimited" => self.space_delimited,
            "PipeDelimited" => self.pipe_delimited,
            "DeepObject" => self.deep_object,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PartUse {
    pub(crate) text: bool,
    pub(crate) file: bool,
    pub(crate) json: bool,
}

impl PartUse {
    const ALL: Self = Self {
        text: true,
        file: true,
        json: true,
    };

    fn contains(self, variant: &syn::Ident) -> bool {
        match variant.to_string().as_str() {
            "Text" => self.text,
            "File" => self.file,
            "Json" => self.json,
            _ => true,
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
) -> proc_macro2::TokenStream {
    let buffered = source(BUFFERED);
    let edges = edge_tokens(
        Edge {
            client,
            server,
            gated,
            precise: false,
        },
        body_limit,
        ClientUse::default(),
        ServerUse::ALL,
    );
    quote::quote! {
        #buffered
        #edges
    }
}

/// Support owned by a generated types crate.
pub(crate) fn types_tokens() -> proc_macro2::TokenStream {
    source(BUFFERED)
}

/// Support owned by a generated client crate.
pub(crate) fn client_tokens(used: ClientUse) -> proc_macro2::TokenStream {
    edge_tokens(
        Edge::CLIENT,
        crate::config::BodyLimit::default(),
        used,
        ServerUse::default(),
    )
}

/// Support owned by a generated server crate.
pub(crate) fn server_tokens(
    body_limit: crate::config::BodyLimit,
    used: ServerUse,
) -> proc_macro2::TokenStream {
    edge_tokens(Edge::SERVER, body_limit, ClientUse::default(), used)
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    client: bool,
    server: bool,
    gated: bool,
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
    server_use: ServerUse,
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
    let style = if precise {
        client
            .then(|| style_source(STYLE, Some(client_use), None))
            .or_else(|| server.then(|| style_source(STYLE, None, Some(server_use))))
    } else {
        Some(source(STYLE))
    };
    let multipart = if precise && client && !client_use.multipart {
        None
    } else if precise {
        client
            .then(|| multipart_source(MULTIPART, true, client_use.parts))
            .or_else(|| server.then(|| multipart_source(MULTIPART, false, server_use.parts)))
    } else {
        Some(source(MULTIPART))
    };
    let style = style.map(|style| quote::quote! { pub mod style { #style } });
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
        let wire = client_source(HTTP, client_use);
        quote::quote! {
            #calling
            pub mod wire { #wire }
            #calling
            pub use wire::*;
        }
    });
    let serve = server.then(|| {
        let serve = source(SERVE);
        let router = with_body_limit(ROUTER, body_limit, server_use);
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
        #style

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
fn with_body_limit(
    text: &str,
    limit: crate::config::BodyLimit,
    used: ServerUse,
) -> proc_macro2::TokenStream {
    let Ok(mut file) = syn::parse_file(text) else {
        return text.parse().unwrap_or_default();
    };
    let bytes = limit.0;
    file.items = file
        .items
        .into_iter()
        .filter(|item| !is_test_module(item))
        .map(|mut item| match &mut item {
            syn::Item::Const(konst) if konst.ident == "BODY_LIMIT" => {
                *konst.expr = syn::parse_quote! { #bytes };
                item
            }
            syn::Item::Fn(function) => {
                if let Some(kind) = unused_body_reader(&function.sig.ident, used) {
                    let reason = format!("this description declares no {kind} request body");
                    function
                        .attrs
                        .push(syn::parse_quote! { #[expect(dead_code, reason = #reason)] });
                }
                if let Some(kind) = unused_response_writer(&function.sig.ident, used) {
                    let reason = format!("this description declares no {kind} response body");
                    function
                        .attrs
                        .push(syn::parse_quote! { #[expect(dead_code, reason = #reason)] });
                }
                item
            }
            syn::Item::Impl(implementation) => {
                for implementation_item in &mut implementation.items {
                    let syn::ImplItem::Fn(function) = implementation_item else {
                        continue;
                    };
                    if let Some(location) = unused_parameter_reader(&function.sig.ident, used) {
                        let reason = format!("this description declares no {location} parameter");
                        function
                            .attrs
                            .push(syn::parse_quote! { #[expect(dead_code, reason = #reason)] });
                    }
                }
                item
            }
            _ => item,
        })
        .collect();
    quote::quote! { #file }
}

fn unused_body_reader(name: &syn::Ident, used: ServerUse) -> Option<&'static str> {
    match name.to_string().as_str() {
        "json_body" if !used.json => Some("JSON"),
        "form_body" if !used.form => Some("form-urlencoded"),
        "multipart_body" if !used.multipart => Some("multipart"),
        "text_body" if !used.text => Some("text"),
        "byte_body" if !used.bytes => Some("raw-byte"),
        _ => None,
    }
}

fn unused_parameter_reader(name: &syn::Ident, used: ServerUse) -> Option<&'static str> {
    match name.to_string().as_str() {
        "path_value" if !used.path => Some("path"),
        "query_value" if !used.query => Some("query"),
        "header_value" if !used.header => Some("header"),
        "cookie_value" if !used.cookie => Some("cookie"),
        _ => None,
    }
}

fn unused_response_writer(name: &syn::Ident, used: ServerUse) -> Option<&'static str> {
    match name.to_string().as_str() {
        "respond_text" if !used.text_response => Some("text"),
        "respond_bytes" if !used.byte_response => Some("raw-byte"),
        "respond_raw" if !used.text_response && !used.byte_response => Some("non-JSON"),
        _ => None,
    }
}

/// One shipped file as tokens, without the tests that verify it here.
///
/// The tests belong beside the code they test and have no business in a consumer's crate: they
/// would be compile time nobody asked for, in a module a consumer cannot even read the source of.
fn source(text: &str) -> proc_macro2::TokenStream {
    let Ok(mut file) = syn::parse_file(text) else {
        return text.parse().unwrap_or_default();
    };
    file.items.retain(|item| !is_test_module(item));
    quote::quote! { #file }
}

fn client_source(text: &str, used: ClientUse) -> proc_macro2::TokenStream {
    let Ok(mut file) = syn::parse_file(text) else {
        return text.parse().unwrap_or_default();
    };
    file.items.retain(|item| !is_test_module(item));
    for item in &mut file.items {
        match item {
            syn::Item::Struct(structure) if structure.ident == "NotAForm" && !used.multipart => {
                expect_dead_code(
                    &mut structure.attrs,
                    "this description declares no multipart request body",
                );
            }
            syn::Item::Fn(function) => match function.sig.ident.to_string().as_str() {
                "to_value" if !used.form && !used.multipart => expect_dead_code(
                    &mut function.attrs,
                    "this description declares no form or multipart request body",
                ),
                "decode_text" if !used.text_response => expect_dead_code(
                    &mut function.attrs,
                    "this description declares no text response body",
                ),
                "decode_bytes" if !used.byte_response => expect_dead_code(
                    &mut function.attrs,
                    "this description declares no raw-byte response body",
                ),
                _ => {}
            },
            syn::Item::Impl(implementation)
                if impl_type_is(implementation, "NotAForm") && !used.multipart =>
            {
                for implementation_item in &mut implementation.items {
                    if let syn::ImplItem::Fn(function) = implementation_item
                        && function.sig.ident == "new"
                    {
                        expect_dead_code(
                            &mut function.attrs,
                            "this description declares no multipart request body",
                        );
                    }
                }
            }
            _ => {}
        }
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

fn expect_dead_code(attributes: &mut Vec<syn::Attribute>, reason: &str) {
    attributes.push(syn::parse_quote! { #[expect(dead_code, reason = #reason)] });
}

fn style_source(
    text: &str,
    client: Option<ClientUse>,
    server: Option<ServerUse>,
) -> proc_macro2::TokenStream {
    let Ok(mut file) = syn::parse_file(text) else {
        return text.parse().unwrap_or_default();
    };
    remove_dead_code_file_expectation(&mut file);
    file.items.retain(|item| !is_test_module(item));
    for item in &mut file.items {
        match item {
            syn::Item::Fn(function)
                if !style_function_used(&function.sig.ident, client, server) =>
            {
                expect_dead_code(
                    &mut function.attrs,
                    "this generated edge does not call this serialization direction",
                );
            }
            syn::Item::Enum(enumeration) if enumeration.ident == "Style" => {
                let mut styles = client.map_or_else(
                    || server.map_or(StyleUse::ALL, |used| used.styles),
                    |used| used.styles,
                );
                // Both directions construct the default form and compare against deep-object
                // inside the shared table itself, even when the generated API never writes either
                // variant explicitly.
                styles.form = true;
                styles.deep_object = true;
                for variant in &mut enumeration.variants {
                    if !styles.contains(&variant.ident) {
                        expect_dead_code(
                            &mut variant.attrs,
                            "this description does not use this parameter style",
                        );
                    }
                }
            }
            _ => {}
        }
    }
    quote::quote! { #file }
}

fn style_function_used(
    name: &syn::Ident,
    client: Option<ClientUse>,
    server: Option<ServerUse>,
) -> bool {
    match name.to_string().as_str() {
        "query_pairs" => client.is_some_and(|used| used.query),
        "form_body" => client.is_some_and(|used| used.form),
        "from_query" => server.is_some_and(|used| used.query),
        "query_object" => server.is_some_and(|used| used.form),
        "path_segment" => client.is_some_and(|used| used.path),
        "matrix_segment" => client.is_some_and(|used| used.matrix),
        "header_value" => client.is_some_and(|used| used.header),
        "cookie_pair" => client.is_some_and(|used| used.cookie),
        _ => true,
    }
}

fn multipart_source(text: &str, client: bool, mut parts: PartUse) -> proc_macro2::TokenStream {
    let Ok(mut file) = syn::parse_file(text) else {
        return text.parse().unwrap_or_default();
    };
    remove_dead_code_file_expectation(&mut file);
    file.items.retain(|item| !is_test_module(item));
    // The shared runtime's inference path constructs both defaults. Only `File` depends entirely
    // on the description emitting a corresponding `PartSpec`.
    parts.text = true;
    parts.json = true;
    for item in &mut file.items {
        match item {
            syn::Item::Fn(function) => {
                let opposite_direction = if client {
                    matches!(
                        function.sig.ident.to_string().as_str(),
                        "boundary_of" | "parse"
                    )
                } else {
                    function.sig.ident == "body"
                };
                if opposite_direction {
                    expect_dead_code(
                        &mut function.attrs,
                        "this generated edge does not call this multipart direction",
                    );
                }
            }
            syn::Item::Enum(enumeration) if enumeration.ident == "PartKind" => {
                for variant in &mut enumeration.variants {
                    if !parts.contains(&variant.ident) {
                        expect_dead_code(
                            &mut variant.attrs,
                            "this description does not use this multipart part kind",
                        );
                    }
                }
            }
            _ => {}
        }
    }
    quote::quote! { #file }
}

fn remove_dead_code_file_expectation(file: &mut syn::File) {
    file.attrs.retain(|attribute| {
        !(attribute.path().is_ident("cfg_attr")
            && quote::quote! { #attribute }
                .to_string()
                .contains("dead_code"))
    });
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
        let items = super::items(super::BUFFERED);
        let spelled = quote::quote! { #(#items)* }.to_string();
        assert!(
            !spelled.contains("super ::"),
            "{}",
            indoc::indoc! {"
                buffered.rs names `super::`, which resolves differently once the file is spliced \
                flat into the generated support root"
            }
        );
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
                    let tokens = super::tokens(client, server, gated, BodyLimit::default());
                    syn::parse2::<syn::File>(tokens.clone()).map_err(|error| {
                        eyre::eyre!(
                            "{}",
                            indoc::formatdoc! {"
                                support(client = {client}, server = {server}, gated = {gated}) does \
                                not parse: {error}
                                {tokens}"
                            }
                        )
                    })?;
                }
            }
        }
        // And each half is really there when it was asked for, so the check above is not passing
        // because there was nothing to compose.
        let both = super::tokens(true, true, true, BodyLimit::default()).to_string();
        assert!(both.contains("ResponseValue"), "{both}");
        assert!(both.contains("Rejection"), "{both}");
        assert!(both.contains("cfg (feature = \"client\")"), "{both}");
        assert!(both.contains("cfg (feature = \"server\")"), "{both}");

        let calling = super::tokens(true, false, true, BodyLimit::default()).to_string();
        assert!(calling.contains("ResponseValue"), "{calling}");
        assert!(!calling.contains("Rejection"), "{calling}");

        let serving = super::tokens(false, true, true, BodyLimit::default()).to_string();
        assert!(!serving.contains("ResponseValue"), "{serving}");
        assert!(serving.contains("Rejection"), "{serving}");

        // Both halves need the style table and the multipart writer, so neither is behind a gate.
        assert!(calling.contains("query_pairs"), "{calling}");
        assert!(serving.contains("query_pairs"), "{serving}");

        // In module mode nothing is gated: there is no crate whose feature could turn it on.
        assert!(
            !super::tokens(true, true, false, BodyLimit::default())
                .to_string()
                .contains("cfg (feature"),
        );
    }

    /// The body ceiling is a knob, and the shipped constant is what it turns.
    ///
    /// Carried in from stage 7, where it was a constant with a comment claiming `DefaultBodyLimit`
    /// could raise it — which it cannot: that layer inserts an extension only extractors calling
    /// `with_limited_body` consult, and this reads the body with `to_bytes` and its own number.
    /// Saying so in a comment was not enough, so it became configuration.
    #[test_util::test]
    fn the_body_ceiling_is_the_configured_one() {
        let rendered = super::tokens(false, true, false, BodyLimit(4096)).to_string();
        assert!(
            rendered.contains("BODY_LIMIT : usize = 4096"),
            "the configured ceiling did not reach the shipped constant"
        );
        // And the default is still the default rather than something a caller has to know to set.
        let fallback = super::tokens(false, true, false, BodyLimit::default()).to_string();
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
        const DEFAULTED: &'static [bool] = &[false, false, false];

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

    #[test_util::test]
    fn a_struct_can_be_read_from_a_sequence_the_way_the_derive_reads_one() {
        assert_eq!(
            serde_json::from_str::<Spike>(r#"["a", 1, true]"#)?,
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
        let error = serde_json::from_str::<Spike>(r#"["a"]"#)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(
            error
                .to_string()
                .starts_with("invalid length 1, expected struct Spike with 3 elements"),
            "{error}"
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
