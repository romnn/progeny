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
mod style;

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
    if !client && !server {
        return buffered;
    }
    // The shipped tree keeps **progeny's own module layout**: `style` and `multipart` are submodules
    // here and submodules there, so a path that resolves while progeny compiles resolves in the
    // consumer's crate too. Flattening them was a standing chance for the two to disagree, and the
    // disagreement would surface as a compile error in somebody else's build.
    let style = items(STYLE);
    let multipart = items(MULTIPART);

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
        let wire = items(HTTP);
        quote::quote! {
            #calling
            #[allow(dead_code)]
            pub mod wire { #(#wire)* }
            #calling
            pub use wire::*;
        }
    });
    let serve = server.then(|| {
        let serve = items(SERVE);
        let router = with_body_limit(items(ROUTER), body_limit);
        quote::quote! {
            #serving
            #[allow(dead_code)]
            pub mod serve { #(#serve)* }
            #serving
            #[allow(dead_code)]
            pub mod router { #(#router)* }
            #serving
            pub use serve::*;
            #serving
            pub use router::*;
        }
    });

    quote::quote! {
        #buffered

        #[allow(dead_code)]
        pub mod style { #(#style)* }

        #[allow(dead_code)]
        pub mod multipart { #(#multipart)* }

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
    items: Vec<syn::Item>,
    limit: crate::config::BodyLimit,
) -> impl Iterator<Item = syn::Item> {
    let bytes = limit.0;
    items.into_iter().map(move |item| match item {
        syn::Item::Const(mut konst) if konst.ident == "BODY_LIMIT" => {
            konst.expr = Box::new(syn::parse_quote! { #bytes });
            syn::Item::Const(konst)
        }
        other => other,
    })
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

/// The same, as items alone.
///
/// A file's own `#![doc]` and `#![allow]` are *inner* attributes, and inner attributes may only
/// open a block. Two files nested into one module would put the second file's inner attributes
/// after the first file's items, which is not Rust — and the failure is quiet, because the renderer
/// falls back to emitting unparsed tokens rather than failing a build.
fn items(text: &str) -> Vec<syn::Item> {
    let Ok(file) = syn::parse_file(text) else {
        return Vec::new();
    };
    file.items
        .into_iter()
        .filter(|item| !is_test_module(item))
        .collect()
}

fn is_test_module(item: &syn::Item) -> bool {
    let syn::Item::Mod(module) = item else {
        return false;
    };
    module.attrs.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && quote::ToTokens::to_token_stream(attribute)
                .to_string()
                .contains("test")
    })
}

#[cfg(test)]
mod tests {
    use crate::config::BodyLimit;
    use serde::de::{Deserialize, Deserializer};

    /// The shipped source has to parse as a file, both ways.
    ///
    /// It is checked here because the renderer's fallback is to emit unparsed tokens rather than
    /// fail a build — the right call for a formatting failure, and the reason a *composition*
    /// failure would otherwise reach a consumer as one very long line that happens not to compile.
    #[test]
    fn the_shipped_module_parses_as_a_file() {
        for client in [false, true] {
            for server in [false, true] {
                for gated in [false, true] {
                    let tokens = super::tokens(client, server, gated, BodyLimit::default());
                    syn::parse2::<syn::File>(tokens.clone()).unwrap_or_else(|error| {
                        panic!(
                            "support(client = {client}, server = {server}, gated = {gated}) does \
                             not parse: {error}\n{tokens}"
                        )
                    });
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
    #[test]
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

    #[test]
    fn every_member_is_read_by_its_wire_name() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": 1, "wireName": true}"#).unwrap(),
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
    }

    #[test]
    fn a_bare_option_accepts_an_absent_member() {
        // No `default` attribute anywhere: serde routes a missing member through a deserializer
        // whose `deserialize_option` answers `visit_none`, and this mirrors that.
        assert_eq!(
            parse(r#"{"required": "a", "wireName": false}"#)
                .unwrap()
                .optional,
            None
        );
    }

    #[test]
    fn an_explicit_null_and_an_absent_member_agree() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": null, "wireName": false}"#)
                .unwrap()
                .optional,
            None
        );
    }

    #[test]
    fn a_duplicate_member_is_rejected_rather_than_last_write_wins() {
        // The case a map-based buffer would lose silently, which is why the buffer is a list.
        let error = parse(r#"{"required": "a", "required": "b", "wireName": false}"#).unwrap_err();
        assert!(
            error.to_string().starts_with("duplicate field `required`"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_member_says_which_one() {
        let error = parse(r#"{"wireName": false}"#).unwrap_err();
        assert!(
            error.to_string().starts_with("missing field `required`"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_member_is_ignored_under_that_policy() {
        assert!(
            parse(r#"{"required": "a", "wireName": false, "surprise": {"deep": [1]}}"#).is_ok()
        );
    }

    #[test]
    fn an_unknown_member_is_refused_under_the_other_one() {
        let mut deserializer =
            serde_json::Deserializer::from_str(r#"{"required": "a", "surprise": 1}"#);
        let error = deserializer
            .deserialize_struct(
                <Spike as Assemble>::NAME,
                <Spike as Assemble>::FIELDS,
                BufferVisitor::<Spike>::new(Unknown::Deny),
            )
            .expect_err("an undeclared member should be refused");
        assert!(
            error.to_string().starts_with("unknown field `surprise`"),
            "{error}"
        );
    }

    #[test]
    fn a_struct_can_be_read_from_a_sequence_the_way_the_derive_reads_one() {
        assert_eq!(
            serde_json::from_str::<Spike>(r#"["a", 1, true]"#).unwrap(),
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
        let error = serde_json::from_str::<Spike>(r#"["a"]"#).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid length 1, expected struct Spike with 3 elements"),
            "{error}"
        );
    }

    #[test]
    fn a_member_of_the_wrong_type_reports_it_the_way_the_format_does() {
        let error = parse(r#"{"required": 7, "wireName": false}"#).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid type: integer `7`, expected a string"),
            "{error}"
        );
    }

    #[test]
    fn a_buffered_value_replays_into_any_type() {
        // The machinery has to be able to hand a buffered value to an arbitrary `Deserialize`, or
        // a struct could only hold scalars.
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2)]);
        let replayed: Vec<u64> =
            Vec::deserialize(ContentDeserializer::<serde_json::Error>::new(content)).unwrap();
        assert_eq!(replayed, [1, 2]);

        let content = Content::Map(vec![(
            Content::Str("a"),
            Content::Seq(vec![Content::Bool(true)]),
        )]);
        let replayed: std::collections::BTreeMap<String, Vec<bool>> =
            Deserialize::deserialize(ContentDeserializer::<serde_json::Error>::new(content))
                .unwrap();
        assert_eq!(replayed["a"], [true]);
    }

    #[test]
    fn a_missing_member_of_a_non_optional_type_is_an_error_and_not_a_default() {
        let outcome: Result<i64, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert!(outcome.is_err());
        let outcome: Result<Option<i64>, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert_eq!(outcome.unwrap(), None);
    }
}
