//! The serving side.
//!
//! The mirror of [`super::client`] over the same model, which is the point: one `ApiModel` renders
//! both, so the two cannot disagree about which statuses an operation declares or how a parameter
//! reaches the wire. Where the client *fills in* a template and *encodes* a parameter, this
//! registers the template and decodes the parameter, and both read the same records to do it.
//!
//! Three things are carried by the generated crate's own type system rather than by a check:
//!
//! * **A handler cannot answer with an undeclared status**, because the response enum has no
//!   variant for one.
//! * **A handler cannot be written for a route the router would refuse**, because the trait method
//!   is only emitted for an operation the classifier accepted
//!   ([`crate::api::RegistrableRoute`]).
//! * **Every decoding failure funnels into one `Rejection`**, handed to a provided trait method
//!   with the request's context, so error replies are shaped in one place and overridden in one
//!   place — a trait method rather than a hook system.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::api::{
    ApiModel, BodyContract, HeaderTypes, Location, MemberReading, OperationContract, ParamContract,
    Reading, RegistrableRoute, ResponseArm, ResponseBody, ResponseHeader, StatusPattern,
    type_suffix,
};
use crate::config::Config;
use crate::contract::{Contracts, RustIdent};

use super::types::{docs as docs_of, response_enum_type_path, type_path as type_tokens};

/// Render the server module.
pub(super) fn render(model: &ApiModel, contracts: &Contracts, config: &Config) -> TokenStream {
    // Paired with the route the classifier accepted rather than filtered by it, so that nothing
    // downstream has to re-ask whether an operation is servable or invent an answer when it is not.
    let servable: Vec<(&OperationContract, &RegistrableRoute)> = model
        .operations()
        .iter()
        .filter_map(|operation| Some((operation, operation.registrable.as_ref()?)))
        .collect();

    let methods = servable
        .iter()
        .map(|(operation, _)| trait_method(operation, contracts, config));
    let items = servable
        .iter()
        .map(|(operation, _)| operation_items(operation, contracts, config));
    let routes = router(&servable, config);

    quote! {
        //! The serving side.
        //!
        //! A request is decoded strictly: a handler receives exactly what the description
        //! declares, or the request is rejected before it arrives. A response is answered in
        //! its *read form* (`types::read`), the shape a client decodes into, so a server can
        //! answer with exactly what a vendor would — a required member left out, or an
        //! undeclared one added through `extra` where the type has one — and a test double can
        //! reproduce the drift a client is written to survive. A twinned type converts from
        //! its strict form with `From`, and a struct whose members are all optional derives
        //! `Default`, so a handler fills in what it means to and `..Default::default()` the
        //! rest. That puts what a response carries in the handler's hands rather than the
        //! compiler's.
        //!
        //! Every response variant carries its `headers`: the group of headers its arm declares,
        //! each typed and optional like a read form's member, beside `raw` for every other
        //! header, or a bare `HeaderMap` where the arm declares none.
        //! They are attached only once the body has encoded, so a reply that fails on the way
        //! out is a `500` without them.

        use super::operations;
        use super::support;

        #[doc(inline)]
        pub use super::support::{Rejection, RejectionResponse};

        /// The API this description defines.
        ///
        /// One method per operation the router can register.
        /// `Send + Sync + 'static` is what `axum` needs of a handler's state: shared across
        /// tasks, and outliving the request it is serving.
        /// `Sized` is what `FromRequestParts` needs of the state it hands the
        /// [`Api::Context`] extractor, which is this implementation.
        pub trait Api: ::std::marker::Sized + ::std::marker::Send + ::std::marker::Sync + 'static {
            /// What every operation receives from the request beyond what the description
            /// declares.
            ///
            /// Extracted from the request head with this implementation as the state, before any
            /// parameter or body is decoded, so an authenticating context refuses a request
            /// before a malformed one is rejected, and its own rejection is the reply.
            /// Decoding then reads the head as the extractor left it.
            ///
            /// `type Context = ();` takes nothing.
            /// `type Context = ::axum::http::request::Parts;` takes the whole head, which clones
            /// it with every extension value in it, so a service with large extensions extracts
            /// what it needs or keeps them behind an `Arc`.
            /// An application type implementing `FromRequestParts` parses exactly what its
            /// operations depend on, once, and fails closed in one place.
            type Context: ::axum::extract::FromRequestParts<Self> + ::std::marker::Send + 'static;

            #(#methods)*

            /// What to answer when a request does not match what the description says.
            ///
            /// Provided, and the one place error replies are shaped.
            /// Every generated decoding failure funnels into it — a bad path segment, an
            /// unparsable body, a missing required query parameter, a body of the wrong media
            /// type — so overriding it changes all of them at once.
            /// It sees the request's context, so a reply can carry what the context extracted,
            /// such as a request ID.
            ///
            /// A context that fails to extract answers with its own rejection and never reaches
            /// this, and neither do the router's own `404` and `405`.
            fn on_rejection(
                &self,
                #[expect(
                    unused_variables,
                    reason = "the default reply depends on the rejection alone; an override reads the context"
                )]
                context: &Self::Context,
                rejection: Rejection,
            ) -> ::axum::response::Response {
                ::axum::response::IntoResponse::into_response(RejectionResponse::of(&rejection))
            }
        }

        #(#items)*

        #routes
    }
}

/// One trait method, in the RPITIT form that promises `Send`.
///
/// Written out rather than as `async fn`: `axum` requires a handler's future to be `Send`, and a
/// bare `async fn` in a trait promises nothing about its future. This is the `trait-variant`
/// desugaring, emitted directly so the generated crate needs no macro to read it.
fn trait_method(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(&operation.rust_name);
    let docs = docs_of(&operation.docs, config.docs);
    let response = response_name(operation);
    let arguments = argument_list(operation, contracts, config);
    quote! {
        #docs
        fn #name(
            &self,
            #(#arguments),*
        ) -> impl ::std::future::Future<Output = #response> + ::std::marker::Send;
    }
}

/// The arguments one handler takes: the request context, a struct per location that has
/// parameters, then a body.
fn argument_list(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> Vec<TokenStream> {
    let mut out = vec![quote! { context: Self::Context }];
    for (location, suffix) in LOCATIONS {
        if operation.params_at(location).next().is_none() {
            continue;
        }
        let name = format_ident!("{}", suffix.to_lowercase());
        let ty = group_name(operation, suffix);
        out.push(quote! { #name: #ty });
    }
    if let Some(body) = &operation.body {
        let ty = super::client::body_type(body, contracts, config);
        // A body the document did not mark required arrives as an `Option`, because a request
        // without one is a request the description permits.
        let ty = if body.required() {
            ty
        } else {
            quote! { ::std::option::Option<#ty> }
        };
        out.push(quote! { body: #ty });
    }
    out
}

/// The parameter locations that become grouped structs, in the order a handler reads them.
pub(crate) const LOCATIONS: [(Location, &str); 4] = [
    (Location::Path, type_suffix::PATH),
    (Location::Query, type_suffix::QUERY),
    (Location::Header, type_suffix::HEADER),
    (Location::Cookie, type_suffix::COOKIE),
];

/// Whether the handler must call a deprecated operation or read a deprecated parameter.
fn touches_deprecated(operation: &OperationContract) -> TokenStream {
    if operation.docs.deprecated || operation.params.iter().any(|param| param.docs.deprecated) {
        quote! {
            #[expect(deprecated, reason = "the generated operation deliberately exposes deprecated API")]
        }
    } else {
        TokenStream::new()
    }
}

/// Everything one operation contributes outside the trait: its parameter groups and its response.
fn operation_items(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let groups = LOCATIONS.map(|(location, suffix)| {
        let params: Vec<&ParamContract> = operation.params_at(location).collect();
        if params.is_empty() {
            return TokenStream::new();
        }
        group_struct(operation, suffix, &params, contracts, config)
    });
    let response = response_enum(operation, contracts, config);
    quote! { #(#groups)* #response }
}

/// One location's parameters, as a struct the handler is handed.
///
/// Grouped by location rather than flattened into the method's arguments so that a handler's
/// signature stays readable when a document declares fourteen query parameters — `jellyfin` does —
/// and so that adding one to the description is a field rather than a positional argument.
fn group_struct(
    operation: &OperationContract,
    suffix: &str,
    params: &[&ParamContract],
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = group_name(operation, suffix);
    let doc = format!(
        " The {} parameters of `{}`.",
        suffix.to_lowercase(),
        operation.rust_name
    );
    let fields = params.iter().map(|param| {
        let field = ident(&param.rust_name);
        let ty = type_tokens(&param.ty, contracts, config);
        // Required as the document declared it: an absent optional parameter is `None` and an
        // absent required one is a rejection, decided during extraction rather than here.
        let ty = if param.required {
            ty
        } else {
            quote! { ::std::option::Option<#ty> }
        };
        let docs = docs_of(&param.docs, config.docs);
        let nesting = super::types::type_complexity(&ty);
        quote! { #docs #nesting pub #field: #ty, }
    });
    // `PartialEq` is deliberately absent. A generated type carries only the derives the
    // configuration asked for, and a group struct holding one that lacks `PartialEq` cannot derive
    // it — which is a compile error in the consumer's crate about a convenience nobody requested.
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone)]
        pub struct #name {
            #(#fields)*
        }
    }
}

/// What one operation may answer with, and the header groups its arms carry.
///
/// One variant per declared arm, so a handler that tries to return an undeclared status does not
/// compile. That is the invariant the generated crate's own types carry, which is this project's
/// preserved capability 5 working as designed rather than as a runtime check.
///
/// Every variant is a struct variant with its `headers`, then its `body` where the arm declares
/// content, after the `status` a range or `default` arm is answered with.
fn response_enum(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = response_name(operation);
    let doc = format!(" What `{}` may answer with.", operation.rust_name);
    let arms = &operation.responses.arms;
    let groups = arms
        .iter()
        .chain(&operation.responses.default)
        .filter_map(|arm| {
            let types = arm.header_types.as_ref()?;
            Some(header_group(operation, arm, types, contracts, config))
        });
    let variants = arms.iter().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        let fields = variant_fields(arm, contracts, config);
        let docs = docs_of(&arm.docs, config.docs);
        match arm.status {
            StatusPattern::Exact(_) => quote! { #docs #variant { #fields }, },
            // A range is not a status, so the handler says which one it answers with, as it does
            // for `default`.
            StatusPattern::Range(class) => {
                let range = format!(" Answered with `status`, which is in the `{class}XX` range.");
                quote! {
                    #docs
                    #[doc = ""]
                    #[doc = #range]
                    #variant { #fields },
                }
            }
        }
    });
    let matches = arms.iter().map(|arm| arm_match(arm, arms));

    // The `default` arm carries its status, because `default` means "any status this description
    // did not otherwise claim" and only the handler knows which one it is answering with. Picking a
    // number here would be progeny deciding what an operation's unlisted statuses are; a struct
    // variant makes the handler say. It is still the case that no *undeclared* status can be sent,
    // because a description with no `default` has no variant to say one with.
    let default_variant = operation.responses.default.as_ref().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        let fields = variant_fields(arm, contracts, config);
        quote! {
            /// A status this description does not name individually.
            ///
            /// `default` claims whatever the other arms do not, so the status is the handler's to
            /// choose — there is no single number the description meant.
            #variant { #fields },
        }
    });
    let default_match = operation
        .responses
        .default
        .as_ref()
        .map(|arm| default_match(arm, arms));

    // A description that declared no responses at all has nothing to answer with, and an empty
    // enum is a type no handler can construct. `204` is the only reply left that invents nothing.
    let empty = (arms.is_empty() && operation.responses.default.is_none()).then(|| {
        quote! {
            /// The description declared no responses, so there is nothing but "it worked".
            NoContent { headers: ::axum::http::HeaderMap },
        }
    });
    let empty_match = (arms.is_empty() && operation.responses.default.is_none()).then(|| {
        quote! {
            Self::NoContent { headers } => {
                support::respond_empty(204u16, support::ResponseHeaders::new(headers))
            }
        }
    });

    quote! {
        #(#groups)*

        #[doc = #doc]
        #[derive(Debug, Clone)]
        pub enum #name {
            #empty
            #(#variants)*
            #default_variant
        }

        impl ::axum::response::IntoResponse for #name {
            fn into_response(self) -> ::axum::response::Response {
                match self {
                    #empty_match
                    #(#matches)*
                    #default_match
                }
            }
        }
    }
}

/// An exact or range arm's reply, refusing a status outside a range arm's class.
fn arm_match(arm: &ResponseArm, arms: &[ResponseArm]) -> TokenStream {
    let variant = format_ident!("{}", arm.rust_name.as_str());
    let pattern = variant_pattern(arm);
    match arm.status {
        StatusPattern::Exact(code) => {
            let response = response(arm, &quote! { #code });
            quote! { Self::#variant #pattern => #response, }
        }
        StatusPattern::Range(class) => {
            let response = response(arm, &quote! { status });
            // A code with an exact arm of its own belongs to that arm, whose body a client
            // decodes it by.
            let exact = arms.iter().filter_map(|other| match other.status {
                StatusPattern::Exact(code) if code / 100 == u16::from(class) => Some(code),
                StatusPattern::Exact(_) | StatusPattern::Range(_) => None,
            });
            let class = u16::from(class);
            // A status outside the arm's range would be one the description does not declare,
            // which this server never sends: it is the handler's defect, so a `500`, and one
            // without the arm's headers.
            quote! {
                Self::#variant #pattern => {
                    if status / 100 != #class #(|| status == #exact)* {
                        return ::axum::response::IntoResponse::into_response((
                            ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            ::std::format!(
                                "the handler answered with {status}, which the declared {}XX range does not cover",
                                #class,
                            ),
                        ));
                    }
                    #response
                }
            }
        }
    }
}

/// One variant's fields: the `status` a range or `default` arm is answered with, its `headers`,
/// and its `body` where the arm declares content.
fn variant_fields(arm: &ResponseArm, contracts: &Contracts, config: &Config) -> TokenStream {
    // `default` is `Range(0)` in the model, so this covers it too.
    let status = matches!(arm.status, StatusPattern::Range(_))
        .then(|| quote! { status: ::std::primitive::u16, });
    let headers = arm.header_types.as_ref().map_or_else(
        || quote! { ::axum::http::HeaderMap },
        |types| {
            let group = ident(&types.group);
            quote! { #group }
        },
    );
    let body = has_body(arm).then(|| {
        let ty = response_enum_type_path(&arm.body, contracts, config);
        quote! { body: #ty, }
    });
    quote! { #status headers: #headers, #body }
}

/// The pattern binding one variant's fields, by the names [`response`] reads them by.
fn variant_pattern(arm: &ResponseArm) -> TokenStream {
    let status = matches!(arm.status, StatusPattern::Range(_)).then(|| quote! { status, });
    let body = has_body(arm).then(|| quote! { body, });
    quote! { { #status headers, #body } }
}

fn has_body(arm: &ResponseArm) -> bool {
    !matches!(arm.body, ResponseBody::Empty)
}

/// The `default` arm's reply, refusing a status another arm claims.
fn default_match(arm: &ResponseArm, arms: &[ResponseArm]) -> TokenStream {
    let variant = format_ident!("{}", arm.rust_name.as_str());
    let pattern = variant_pattern(arm);
    let response = response(arm, &quote! { status });
    // `default` is every status no other arm claims, and a client picks the arm by status before
    // it decodes.
    // A `default` sent with a claimed status would be decoded as that arm's body, so it is the
    // handler's defect, a `500` without the arm's headers, as it is for a range arm.
    let guard = claimed_pattern(arms).map(|claimed| {
        quote! {
            if ::std::matches!(status, #claimed) {
                return ::axum::response::IntoResponse::into_response((
                    ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ::std::format!(
                        "the handler answered `default` with {status}, which another declared response claims",
                    ),
                ));
            }
        }
    });
    quote! {
        Self::#variant #pattern => {
            #guard
            #response
        }
    }
}

/// The statuses the exact and range arms claim, as one pattern, or `None` when there are none.
///
/// Read from the same finalized arms the client's status match is rendered from, so the two
/// halves agree about which statuses `default` owns.
/// Claims are merged into disjoint, non-adjacent spans before they are spelled: an exact code
/// inside a range arm's class is already covered, and an alternative another one covers, or one
/// that continues it, is a warning in the generated crate.
fn claimed_pattern(arms: &[ResponseArm]) -> Option<TokenStream> {
    let mut claims: Vec<std::ops::RangeInclusive<u16>> = arms
        .iter()
        .map(|arm| match arm.status {
            StatusPattern::Exact(code) => code..=code,
            StatusPattern::Range(class) => {
                let low = u16::from(class) * 100;
                low..=low + 99
            }
        })
        .collect();
    claims.sort_by_key(|claim| *claim.start());
    let mut spans: Vec<std::ops::RangeInclusive<u16>> = Vec::new();
    for claim in claims {
        match spans.last_mut() {
            Some(span) if *claim.start() <= span.end().saturating_add(1) => {
                *span = *span.start()..=*span.end().max(claim.end());
            }
            _ => spans.push(claim),
        }
    }
    let alternatives = spans.iter().map(|span| {
        let (low, high) = (*span.start(), *span.end());
        if low == high {
            quote! { #low }
        } else {
            quote! { #low..=#high }
        }
    });
    (!spans.is_empty()).then(|| quote! { #(#alternatives)|* })
}

/// One arm's reply, from the `headers` and `body` its pattern binds.
///
/// The writer is handed the headers and attaches them only once the body has encoded, so every
/// way the reply can fail on the way out is a `500` without them.
fn response(arm: &ResponseArm, status: &TokenStream) -> TokenStream {
    let headers = if arm.header_types.is_some() {
        quote! { headers.into_response_headers() }
    } else {
        quote! { support::ResponseHeaders::new(headers) }
    };
    match &arm.body {
        ResponseBody::Json { content_type, .. } => {
            quote! { support::respond_json(#status, #content_type, &body, #headers) }
        }
        ResponseBody::Empty => quote! { support::respond_empty(#status, #headers) },
        ResponseBody::Text { content_type } => {
            quote! { support::respond_text(#status, #content_type, *body, #headers) }
        }
        ResponseBody::Bytes { content_type } => {
            quote! { support::respond_bytes(#status, #content_type, *body, #headers) }
        }
    }
}

/// The struct an arm's declared headers are answered through, named `types.group`.
///
/// Every typed field is an `Option` whatever the description requires, for the reason a response
/// body is answered in its read form: a server can then leave out what a vendor would.
/// Lives in the server module because it names `axum`'s header map, which the shared types crate
/// does not depend on.
fn header_group(
    operation: &OperationContract,
    arm: &ResponseArm,
    types: &HeaderTypes,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(&types.group);
    let doc = format!(
        " The headers `{}` declares for its {} response.",
        operation.rust_name,
        status_label(arm)
    );
    let fields = arm.headers.iter().map(|header| {
        let field = ident(&header.rust_name);
        let ty = type_tokens(&header.ty, contracts, config);
        let ty = quote! { ::std::option::Option<#ty> };
        let nesting = super::types::type_complexity(&ty);
        let docs = header_docs(header, config);
        quote! { #docs #nesting pub #field: #ty, }
    });
    // Each typed header is handed over as JSON, which the support runtime spells in one place,
    // with whether the description declares it a list, which a JSON value alone cannot say.
    let writes = arm.headers.iter().map(|header| {
        let field = ident(&header.rust_name);
        let wire = &header.wire_name;
        let list = header.style.array();
        let value = if header.style.content() {
            quote! { ::serde_json::to_string(value).map(::serde_json::Value::String) }
        } else {
            quote! { ::serde_json::to_value(value) }
        };
        quote! {
            if let ::std::option::Option::Some(value) = &self.#field {
                headers.typed(#wire, #list, #value);
            }
        }
    });
    let deprecated = arm.headers.iter().any(|header| header.docs.deprecated).then(|| {
        quote! {
            #[expect(deprecated, reason = "the description still declares the deprecated header, so it is still written")]
        }
    });
    quote! {
        #[doc = #doc]
        ///
        /// Every typed field is optional, as a response's read form is, so a handler can leave out
        /// a header the description requires.
        /// `raw` is sent as it is, and a name in it replaces the typed field of that name.
        #[derive(Debug, Clone, Default)]
        pub struct #name {
            #(#fields)*
            /// Headers sent as they are: undeclared ones, and declared ones the typed fields do
            /// not spell.
            ///
            /// Every value of a name here replaces the typed field of that name, in order.
            /// A `Content-Type` here replaces the media type the arm declares.
            pub raw: ::axum::http::HeaderMap,
        }

        impl #name {
            /// The typed fields in their `simple` spelling, with `raw` over them.
            #deprecated
            fn into_response_headers(self) -> support::ResponseHeaders {
                let mut headers = support::ResponseHeaders::new(self.raw);
                #(#writes)*
                headers
            }
        }
    }
}

/// One typed header's doc comment: the wire name its field writes, then what the description
/// says about it or, where it says nothing, where it is declared.
fn header_docs(header: &ResponseHeader, config: &Config) -> TokenStream {
    let name = format!(" The `{}` header.", header.wire_name);
    let prose = super::types::docs_prose(&header.docs, config.docs);
    let described = if prose.is_empty() {
        let origin = format!(" Generated from `{}`.", header.origin);
        quote! { #[doc = #origin] }
    } else {
        prose
    };
    let required = header.required.then(|| {
        quote! {
            ///
            /// The description requires it; a handler can still leave it out, as a server drifting
            /// from the description would.
        }
    });
    let deprecated = header.docs.deprecated.then(|| quote! { #[deprecated] });
    quote! {
        #[doc = #name]
        #[doc = ""]
        #described
        #required
        #deprecated
    }
}

/// `fn router<A: Api>(api: A) -> axum::Router`.
///
/// Registers exactly the routes the classifier accepted, which is why the `axum`-panics-at-startup
/// failure mode is unreachable rather than unlikely: a route the router would refuse never became
/// a trait method in the first place.
fn router(servable: &[(&OperationContract, &RegistrableRoute)], config: &Config) -> TokenStream {
    // Grouped by path so that two methods on one route are one `route()` call with two handlers,
    // which is what `axum` requires — registering the same path twice panics even when the methods
    // differ.
    let mut by_path: std::collections::BTreeMap<
        &str,
        Vec<(&OperationContract, &RegistrableRoute)>,
    > = std::collections::BTreeMap::new();
    for (operation, route) in servable {
        by_path
            .entry(route.path())
            .or_default()
            .push((operation, route));
    }

    let registrations = by_path.values().filter_map(|group| {
        // The template is spelled through the reflection rather than as a literal, and as an
        // inline `const` so that it *is* a literal by the time the router runs: `Route::X.path()`
        // is a `const fn` over the same `RegistrableRoute` string, so the router and the
        // `operations` module cannot register different templates, removing or renaming a route
        // there fails this compile, and nothing is interpreted at startup. The first operation of
        // the group names it; the rest share the template by construction.
        let (first, _) = group.first()?;
        let route = super::operations::variant(first);
        let handlers = group.iter().map(|(operation, route)| {
            let verb = format_ident!("{}", route.method().slug());
            let handler = handler_name(operation);
            quote! { ::axum::routing::#verb(#handler::<A>) }
        });
        // `MethodRouter::merge` rather than chained builder calls: the chained form needs the
        // methods in a fixed order to typecheck, and the document supplies whatever order it likes.
        Some(quote! {
            router = router.route(
                const { operations::Route::#route.path() },
                ::axum::routing::MethodRouter::new()#(.merge(#handlers))*,
            );
        })
    });

    let handlers = servable
        .iter()
        .map(|(operation, _)| handler_fn(operation, config));

    // `http` names its constants by the request-line token, so the reflection's method table is
    // this table too.
    let http_methods = super::operations::METHODS.map(|(variant, token)| {
        let variant = format_ident!("{variant}");
        let constant = format_ident!("{token}");
        quote! { operations::Method::#variant => ::axum::http::Method::#constant, }
    });

    quote! {
        /// A router serving every operation this description declares that a router can register.
        ///
        /// Routes the router would have refused were dropped at generation time, each with a
        /// diagnostic naming it, so this cannot panic on a conflicting or malformed route.
        pub fn router<A: Api>(api: A) -> ::axum::Router {
            let api = ::std::sync::Arc::new(api);
            let mut router = ::axum::Router::new();
            #(#registrations)*
            router.with_state(api)
        }

        /// The `http` method for a declared one, for keying axum extractors and middleware.
        ///
        /// A free function rather than a `From` impl: under Workspace packaging both types are
        /// foreign to this crate, and the impl would be an orphan-rule error. The client module
        /// carries the same bridge to `reqwest::Method`, which is this type under another name.
        #[must_use]
        pub fn http_method(method: operations::Method) -> ::axum::http::Method {
            match method {
                #(#http_methods)*
            }
        }

        #(#handlers)*
    }
}

/// A parameter's extracted value, read as JSON where the parameter is declared with a JSON
/// `content`: such a parameter arrives as that document's text.
fn content_source(param: &ParamContract, source: TokenStream) -> TokenStream {
    if !param.style.content() {
        return source;
    }
    quote! {
        (#source).map(|found| {
            found.map(|value| match value {
                ::serde_json::Value::String(text) => ::serde_json::from_str(&text)
                    .unwrap_or(::serde_json::Value::String(text)),
                other => other,
            })
        })
    }
}

/// A reading plan as the `support::Reading` value that carries it.
///
/// Spelled inline at each position rather than named once, because the common plans are a single
/// unit variant and a named table would cost more source than it saves.
/// Every part is a constant expression, so the reference the reader takes is promoted to
/// `'static`.
fn reading_tokens(reading: &Reading) -> TokenStream {
    match reading {
        Reading::Keep => quote! { support::Reading::Keep },
        Reading::Spelled => quote! { support::Reading::Spelled },
        Reading::Items(row) => {
            let row = reading_tokens(row);
            quote! { support::Reading::Items(&#row) }
        }
        Reading::Tuple(rows) => {
            let rows = rows.iter().map(reading_tokens);
            quote! { support::Reading::Tuple(&[#(#rows),*]) }
        }
        Reading::Object { members, others } => {
            let members = members.iter().map(|MemberReading { wire_name, reading }| {
                let reading = reading_tokens(reading);
                quote! { (#wire_name, #reading) }
            });
            let others = reading_tokens(others);
            quote! {
                support::Reading::Object { members: &[#(#members),*], others: &#others }
            }
        }
        Reading::Whole(row) => {
            let row = reading_tokens(row);
            quote! { support::Reading::Whole(&#row) }
        }
    }
}

/// The `axum` handler that stands between a request and one trait method.
///
/// Generated as a free function per operation rather than a closure so that its extraction is
/// readable in the emitted source, which is the thing a reader of a checked-in generated crate is
/// actually going to look at.
fn handler_fn(operation: &OperationContract, config: &Config) -> TokenStream {
    let name = handler_name(operation);
    let method = ident(&operation.rust_name);
    let response = response_name(operation);
    let operation_label = operation.rust_name.as_str();
    let deprecated = touches_deprecated(operation);

    let mut extractions = Vec::new();
    let mut arguments = Vec::new();
    for (location, suffix) in LOCATIONS {
        let params: Vec<&ParamContract> = operation.params_at(location).collect();
        if params.is_empty() {
            continue;
        }
        let binding = format_ident!("{}", suffix.to_lowercase());
        let group = group_name(operation, suffix);
        let reads = params.iter().map(|param| {
            let field = ident(&param.rust_name);
            let wire = &param.wire_name;
            let required = param.required;
            let style = super::client::style_tokens(param.style.style());
            let explode = param.style.explode();
            // The schema's shape rides along because the wire alone cannot say whether `ids=a,b`
            // is an array or a scalar with a comma in it.
            let array = param.style.array();
            let object = param.style.object();
            let reading = reading_tokens(&param.reading);
            let source = match location {
                Location::Path => {
                    quote! {
                        ::std::result::Result::<_, ::std::convert::Infallible>::Ok(
                            incoming.path_value(#wire, #style, #explode, #array, #object)
                        )
                    }
                }
                Location::Query => {
                    let reserved = param.style.allow_reserved();
                    quote! {
                        ::std::result::Result::<_, ::std::convert::Infallible>::Ok(
                            incoming.query_value(
                                #wire,
                                support::style::QueryRow {
                                    style: #style,
                                    explode: #explode,
                                    array: #array,
                                    reserved: #reserved,
                                    object: #object,
                                },
                            )
                        )
                    }
                }
                Location::Header => {
                    quote! { incoming.header_value(#wire, #explode, #array, #object) }
                }
                Location::Cookie => quote! { incoming.cookie_value(#wire, #array, #object) },
            };
            let source = content_source(param, source);
            quote! {
                #field: match support::read(#source, #wire, #required, #operation_label, &#reading) {
                    ::std::result::Result::Ok(value) => value,
                    ::std::result::Result::Err(rejection) => {
                        return api.on_rejection(&context, rejection);
                    }
                },
            }
        });
        extractions.push(quote! { let #binding = #group { #(#reads)* }; });
        arguments.push(quote! { #binding });
    }

    if let Some(body) = &operation.body {
        let read = body_extraction(
            body,
            operation_label,
            config,
            super::client::content_type_param(operation).is_some(),
        );
        extractions.push(read);
        arguments.push(quote! { body });
    }

    let head = request_head(!extractions.is_empty());
    quote! {
        #deprecated
        async fn #name<A: Api>(
            ::axum::extract::State(api): ::axum::extract::State<::std::sync::Arc<A>>,
            request: ::axum::extract::Request,
        ) -> ::axum::response::Response {
            use ::axum::response::IntoResponse as _;
            #head
            #(#extractions)*
            let response: #response = api.#method(context, #(#arguments),*).await;
            response.into_response()
        }
    }
}

/// Splitting the request, extracting the context, and taking the rest apart for decoding.
///
/// The body is bound and the request decoded only when something reads from them: an operation
/// with no parameters and no body would otherwise bind values it never touches, which is a warning
/// in the consumer's own build about code they did not write.
fn request_head(decodes: bool) -> TokenStream {
    let body = if decodes {
        quote! { body }
    } else {
        quote! { _ }
    };
    let receive = decodes.then(|| {
        quote! {
            // Taken apart once, so a fourteen-parameter operation makes one pass over the query
            // string rather than fourteen.
            let incoming = support::receive(parts, body).await;
        }
    });
    quote! {
        let (mut parts, #body) = request.into_parts();
        // First, so an authenticating context refuses a request before a malformed one is
        // rejected.
        // Its rejection is its own reply: no context exists to hand `on_rejection`.
        let context = match <A::Context as ::axum::extract::FromRequestParts<A>>::from_request_parts(
            &mut parts,
            &*api,
        )
        .await
        {
            ::std::result::Result::Ok(context) => context,
            ::std::result::Result::Err(rejection) => return rejection.into_response(),
        };
        #receive
    }
}

/// Reading the request body into whatever the description said it is.
fn body_extraction(
    body: &BodyContract,
    operation: &str,
    config: &Config,
    content_type_declared: bool,
) -> TokenStream {
    let required = body.required();
    let (declared, read) = match body {
        BodyContract::Json {
            content_type,
            accepts,
            ..
        } => (
            accepts.clone().unwrap_or_else(|| content_type.clone()),
            quote! { support::json_body(incoming).await },
        ),
        BodyContract::Form { specs, reading, .. } => {
            let reading = reading_tokens(reading);
            let specs = super::client::form_specs(specs);
            (
                "application/x-www-form-urlencoded".to_owned(),
                quote! { support::form_body(incoming, #specs, &#reading).await },
            )
        }
        BodyContract::Multipart { parts, reading, .. } => {
            let reading = reading_tokens(reading);
            let specs = super::client::part_specs(parts);
            (
                "multipart/form-data".to_owned(),
                quote! { support::multipart_body(incoming, #specs, &#reading).await },
            )
        }
        BodyContract::Text { content_type, .. } => (
            content_type.clone(),
            quote! { support::text_body(incoming).await },
        ),
        BodyContract::Bytes {
            content_type,
            accepts,
            ..
        } => {
            // The handler's argument comes from `client::body_type`, which under
            // `formats.bytes = "bytes"` says `::bytes::Bytes`; the reader hands back `Vec<u8>`,
            // so the seam converts — or the generated crate does not compile.
            let read = match config.formats.bytes {
                crate::config::BytesRepr::Bytes => quote! {
                    support::byte_body(incoming)
                        .await
                        .map(|body| body.map(::bytes::Bytes::from))
                },
                crate::config::BytesRepr::Vec => quote! { support::byte_body(incoming).await },
            };
            (
                accepts.clone().unwrap_or_else(|| content_type.clone()),
                read,
            )
        }
    };
    let reader = if required {
        quote! { support::required_body }
    } else {
        quote! { support::optional_body }
    };
    // The gate stands down when the description declares its own `Content-Type` header
    // parameter: the document made the request's type caller data (cloudflare's R2 object
    // upload stores it), and refusing what the description invites would reject the generated
    // client's own requests.
    // A wildcard admits what it matches, and the type the generated client sends in its place.
    let admitted = match body {
        BodyContract::Json {
            content_type,
            accepts: Some(wildcard),
            ..
        }
        | BodyContract::Bytes {
            content_type,
            accepts: Some(wildcard),
            ..
        } => vec![wildcard.clone(), content_type.clone()],
        _ => vec![declared.clone()],
    };
    let gate = (!content_type_declared).then(|| {
        quote! {
            // The declared media type is part of the contract: a body claiming another type is
            // refused `415` before it is parsed, which is the same protection `axum::Json`
            // gives a hand-written handler against a cross-origin form's `text/plain` "simple
            // request".
            if let ::std::option::Option::Some(sent) = incoming.mismatched_media_type(&[#(#admitted),*]) {
                return api.on_rejection(
                    &context,
                    support::Rejection::UnsupportedMediaType {
                        operation: #operation,
                        declared: #declared,
                        sent,
                    },
                );
            }
        }
    });
    quote! {
        #gate
        let body = match #reader(#read, #operation) {
            ::std::result::Result::Ok(value) => value,
            ::std::result::Result::Err(rejection) => {
                return api.on_rejection(&context, rejection);
            }
        };
    }
}

pub(crate) fn group_name(operation: &OperationContract, suffix: &str) -> proc_macro2::Ident {
    format_ident!("{}{suffix}", operation.rust_name.type_stem())
}

pub(crate) fn response_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!(
        "{}{}",
        operation.rust_name.type_stem(),
        type_suffix::RESPONSE
    )
}

/// The status an arm answers, as its doc comments spell it: `` `202` ``, `` `4XX` `` or
/// `` `default` ``.
pub(crate) fn status_label(arm: &ResponseArm) -> String {
    match arm.status {
        StatusPattern::Exact(code) => format!("`{code}`"),
        StatusPattern::Range(0) => "`default`".to_owned(),
        StatusPattern::Range(class) => format!("`{class}XX`"),
    }
}

fn handler_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!("serve_{}", operation.rust_name.as_str())
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
