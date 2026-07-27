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
//! * **Every extraction failure funnels into one `Rejection`**, handed to a provided trait method,
//!   so error bodies are shaped in one place and overridden in one place — a trait method rather
//!   than a hook system.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::api::{
    ApiModel, BodyContract, Location, OperationContract, ParamContract, RegistrableRoute,
    StatusPattern,
};
use crate::config::Config;
use crate::contract::{Contracts, RustIdent};

use super::client::capitalize;
use super::types::{docs as docs_of, type_path as type_tokens};

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
    let routes = router(&servable, contracts);

    quote! {
        #![doc = " The serving side."]

        use super::support;

        #[doc(inline)]
        pub use super::support::{Rejection, RejectionResponse};

        /// The API this description defines.
        ///
        /// One method per operation the router can register. The bounds are what `axum` needs of a
        /// handler's state: shared across tasks, and outliving the request it is serving.
        pub trait Api: ::std::marker::Send + ::std::marker::Sync + 'static {
            #(#methods)*

            /// What to answer when a request does not match what the description says.
            ///
            /// Provided, and the one place error bodies are shaped. Every generated extractor
            /// funnels into it — a bad path segment, an unparsable body, a missing required
            /// query parameter — so overriding it changes all of them at once.
            fn on_rejection(&self, rejection: Rejection) -> RejectionResponse {
                RejectionResponse::of(&rejection)
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
    let docs = docs_of(&operation.docs);
    let response = response_name(operation);
    let deprecated = touches_deprecated(operation, contracts);
    let arguments = argument_list(operation, contracts, config);
    quote! {
        #docs
        #deprecated
        fn #name(
            &self,
            #(#arguments),*
        ) -> impl ::std::future::Future<Output = #response> + ::std::marker::Send;
    }
}

/// The grouped arguments one handler takes: a struct per location that has parameters, then a body.
fn argument_list(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> Vec<TokenStream> {
    let mut out = Vec::new();
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
    (Location::Path, "Path"),
    (Location::Query, "Query"),
    (Location::Header, "Header"),
    (Location::Cookie, "Cookie"),
];

/// Whether anything this operation names is deprecated.
///
/// One answer for all of an operation's items, because the allowance has to sit on the *item* and
/// an operation owns several. The same lesson the client renderer learned: a field-level `#[allow]`
/// silences the field and not the derive that expands at the same span, so the warning count halves
/// and the gate stays red. Two ways to touch a deprecated thing here — a parameter or a response
/// naming a deprecated *type*, and a parameter the document deprecated *itself*, which becomes a
/// deprecated field the handler then reads.
fn touches_deprecated(operation: &OperationContract, contracts: &Contracts) -> TokenStream {
    if operation.docs.deprecated || operation.params.iter().any(|param| param.docs.deprecated) {
        return quote! { #[allow(deprecated)] };
    }
    super::types::deprecated_use(super::client::operation_types(operation), contracts)
}

/// Everything one operation contributes outside the trait: its parameter groups and its response.
fn operation_items(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let allow = touches_deprecated(operation, contracts);
    let groups = LOCATIONS.map(|(location, suffix)| {
        let params: Vec<&ParamContract> = operation.params_at(location).collect();
        if params.is_empty() {
            return TokenStream::new();
        }
        group_struct(operation, suffix, &params, contracts, config, &allow)
    });
    let response = response_enum(operation, contracts, config, &allow);
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
    allow: &TokenStream,
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
        let docs = docs_of(&param.docs);
        quote! { #docs pub #field: #ty, }
    });
    // `PartialEq` is deliberately absent. A generated type carries only the derives the
    // configuration asked for, and a group struct holding one that lacks `PartialEq` cannot derive
    // it — which is a compile error in the consumer's crate about a convenience nobody requested.
    let nesting = quote! { #[allow(clippy::type_complexity)] };
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone)]
        #allow
        #nesting
        pub struct #name {
            #(#fields)*
        }
    }
}

/// What one operation may answer with.
///
/// One variant per declared arm, so a handler that tries to return an undeclared status does not
/// compile. That is the invariant the generated crate's own types carry, which is this project's
/// preserved capability 5 working as designed rather than as a runtime check.
fn response_enum(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
    allow: &TokenStream,
) -> TokenStream {
    let name = response_name(operation);
    let doc = format!(" What `{}` may answer with.", operation.rust_name);
    let arms = &operation.responses.arms;
    let variants = arms.iter().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        let ty = type_tokens(&arm.ty, contracts, config);
        let docs = docs_of(&arm.docs);
        quote! { #docs #variant(#ty), }
    });
    let matches = arms.iter().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        let status = status_code(arm.status);
        quote! { Self::#variant(body) => support::router::respond(#status, &body), }
    });

    // The `default` arm carries its status, because `default` means "any status this description
    // did not otherwise claim" and only the handler knows which one it is answering with. Picking a
    // number here would be progeny deciding what an operation's unlisted statuses are; a struct
    // variant makes the handler say. It is still the case that no *undeclared* status can be sent,
    // because a description with no `default` has no variant to say one with.
    let default_variant = operation.responses.default.as_ref().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        let ty = type_tokens(&arm.ty, contracts, config);
        quote! {
            /// A status this description does not name individually.
            ///
            /// `default` claims whatever the other arms do not, so the status is the handler's to
            /// choose — there is no single number the description meant.
            #variant { status: ::std::primitive::u16, body: #ty },
        }
    });
    let default_match = operation.responses.default.as_ref().map(|arm| {
        let variant = format_ident!("{}", arm.rust_name.as_str());
        quote! { Self::#variant { status, body } => support::router::respond(status, &body), }
    });

    // A description that declared no responses at all has nothing to answer with, and an empty
    // enum is a type no handler can construct. `204` is the only reply left that invents nothing.
    let empty = (arms.is_empty() && operation.responses.default.is_none()).then(|| {
        quote! {
            /// The description declared no responses, so there is nothing but "it worked".
            NoContent,
        }
    });
    let empty_match = (arms.is_empty() && operation.responses.default.is_none())
        .then(|| quote! { Self::NoContent => support::router::respond(204u16, &()), });

    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone)]
        #allow
        #[allow(clippy::large_enum_variant)]
        pub enum #name {
            #empty
            #(#variants)*
            #default_variant
        }

        #allow
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

/// The number a declared arm answers with.
fn status_code(status: StatusPattern) -> u16 {
    match status {
        StatusPattern::Exact(code) => code,
        // The first code the range covers, because a range is not a status and a server has to send
        // one number. `4XX` becomes `400`, which is the reading every server already gives it.
        StatusPattern::Range(class) => u16::from(class) * 100,
    }
}

/// `fn router<A: Api>(api: A) -> axum::Router`.
///
/// Registers exactly the routes the classifier accepted, which is why the `axum`-panics-at-startup
/// failure mode is unreachable rather than unlikely: a route the router would refuse never became
/// a trait method in the first place.
fn router(
    servable: &[(&OperationContract, &RegistrableRoute)],
    contracts: &Contracts,
) -> TokenStream {
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

    let registrations = by_path.iter().map(|(path, operations)| {
        let handlers = operations.iter().map(|(operation, route)| {
            let verb = format_ident!("{}", route.method().slug());
            let handler = handler_name(operation);
            quote! { ::axum::routing::#verb(#handler::<A>) }
        });
        // `MethodRouter::merge` rather than chained builder calls: the chained form needs the
        // methods in a fixed order to typecheck, and the document supplies whatever order it likes.
        quote! {
            router = router.route(#path, ::axum::routing::MethodRouter::new()#(.merge(#handlers))*);
        }
    });

    let handlers = servable
        .iter()
        .map(|(operation, _)| handler_fn(operation, contracts));

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

        #(#handlers)*
    }
}

/// The `axum` handler that stands between a request and one trait method.
///
/// Generated as a free function per operation rather than a closure so that its extraction is
/// readable in the emitted source, which is the thing a reader of a checked-in generated crate is
/// actually going to look at.
fn handler_fn(operation: &OperationContract, contracts: &Contracts) -> TokenStream {
    let name = handler_name(operation);
    let method = ident(&operation.rust_name);
    let response = response_name(operation);
    let operation_label = operation.rust_name.as_str();
    let deprecated = touches_deprecated(operation, contracts);

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
            let source = match location {
                Location::Path => quote! { incoming.path_value(#wire) },
                Location::Query => quote! { incoming.query_value(#wire, #style, #explode, #array) },
                Location::Header => quote! { incoming.header_value(#wire) },
                Location::Cookie => quote! { incoming.cookie_value(#wire) },
            };
            quote! {
                #field: match support::read(#source, #wire, #required, #operation_label) {
                    ::std::result::Result::Ok(value) => value,
                    ::std::result::Result::Err(rejection) => {
                        return api.on_rejection(rejection).into_response();
                    }
                },
            }
        });
        extractions.push(quote! { let #binding = #group { #(#reads)* }; });
        arguments.push(quote! { #binding });
    }

    if let Some(body) = &operation.body {
        let read = body_extraction(body, operation_label);
        extractions.push(read);
        arguments.push(quote! { body });
    }

    // The request is taken apart only when something reads from it. An operation with no
    // parameters and no body would otherwise bind a value it never touches, which is a warning in
    // the consumer's own build about code they did not write.
    let receive = (!extractions.is_empty()).then(|| {
        quote! {
            // Taken apart once, so a fourteen-parameter operation makes one pass over the query
            // string rather than fourteen.
            let incoming = support::router::receive(request).await;
        }
    });
    let unused = extractions.is_empty().then(|| quote! { let _ = request; });
    quote! {
        #deprecated
        async fn #name<A: Api>(
            ::axum::extract::State(api): ::axum::extract::State<::std::sync::Arc<A>>,
            request: ::axum::extract::Request,
        ) -> ::axum::response::Response {
            use ::axum::response::IntoResponse as _;
            #unused
            #receive
            #(#extractions)*
            let response: #response = api.#method(#(#arguments),*).await;
            response.into_response()
        }
    }
}

/// Reading the request body into whatever the description said it is.
fn body_extraction(body: &BodyContract, operation: &str) -> TokenStream {
    let required = body.required();
    let read = match body {
        BodyContract::Json { .. } => quote! { support::router::json_body(incoming).await },
        BodyContract::Form { specs, .. } => {
            let specs = super::client::form_specs(specs);
            quote! { support::router::form_body(incoming, #specs).await }
        }
        BodyContract::Multipart { parts, .. } => {
            let specs = super::client::part_specs(parts);
            quote! { support::router::multipart_body(incoming, #specs).await }
        }
        BodyContract::Text { .. } => quote! { support::router::text_body(incoming).await },
        BodyContract::Bytes { .. } => quote! { support::router::byte_body(incoming).await },
    };
    let reader = if required {
        quote! { support::required_body }
    } else {
        quote! { support::optional_body }
    };
    quote! {
        let body = match #reader(#read, #operation) {
            ::std::result::Result::Ok(value) => value,
            ::std::result::Result::Err(rejection) => {
                return api.on_rejection(rejection).into_response();
            }
        };
    }
}

pub(crate) fn group_name(operation: &OperationContract, suffix: &str) -> proc_macro2::Ident {
    format_ident!("{}{suffix}", type_stem(operation))
}

pub(crate) fn response_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!("{}Response", type_stem(operation))
}

fn handler_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!("serve_{}", operation.rust_name.as_str())
}

/// The upper-camel form of an operation's name, which every type it owns is built from.
fn type_stem(operation: &OperationContract) -> String {
    operation
        .rust_name
        .as_str()
        .split('_')
        .map(capitalize)
        .collect()
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
