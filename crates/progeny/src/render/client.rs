//! The calling side: one `Client`, one builder per operation.
//!
//! Everything here is transcription. Which media type a body uses, how a parameter serializes,
//! which status arms exist and in what order — all of it was decided in [`crate::api`] and is
//! sitting in a record; this turns records into tokens and makes no choice of its own.
//!
//! Two shapes are worth knowing before reading the output.
//!
//! **Builders are runtime-checked, not typestate.** A required parameter is an `Option` field with
//! a plain setter, and `send()` refuses when one was never set. Typestate would move that refusal
//! to compile time at the cost of a type parameter per required field — which is a per-operation
//! compile cost multiplied by an operation count that reaches four figures in this corpus. The
//! tie-breaker this project always uses is measurement, and the measurement is in
//! `cargo xtask bench-compile --builders`.
//!
//! **A per-operation response type is emitted only when the document needs one.** One success arm
//! yields that arm's type directly; several yield an enum. The same for errors. An operation with
//! one `200` and one `404` therefore adds no types at all, which is the overwhelmingly common case
//! and the reason the client does not double the size of a generated crate.

use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::api::{
    ApiModel, BodyContract, FormSpec, Location, OperationContract, ParamContract, PartKind,
    PartSpec, Piece, ResponseArm, Style,
};
use crate::config::{BytesRepr, Config};
use crate::contract::{Contracts, RustIdent};

use super::types::{docs as docs_of, type_path as type_tokens};

/// Render the client module.
pub(super) fn render(model: &ApiModel, contracts: &Contracts, config: &Config) -> TokenStream {
    let base = default_base_url(model);
    let methods = model.operations().iter().map(|operation| {
        let name = ident(&operation.rust_name);
        let builder = builder_name(operation);
        let docs = docs_of(&operation.docs);
        // A deprecated operation's builder is `#[deprecated]`, and this accessor both returns it and
        // constructs it — two uses rustc lints, in the consumer's crate, about code the consumer did
        // not write. The method carrying `#[deprecated]` itself exempts neither: being deprecated is
        // not a licence to name deprecated things. The builder's own `impl` already carries the same
        // allowance for the same reason.
        let allow = operation
            .docs
            .deprecated
            .then(|| quote! { #[allow(deprecated)] });
        quote! {
            #docs
            #allow
            #[must_use]
            pub fn #name(&self) -> #builder<'_> {
                #builder::new(self)
            }
        }
    });

    let builders = model
        .operations()
        .iter()
        .map(|operation| builder(operation, contracts, config));

    quote! {
        //! The calling side.

        use super::support;

        #[doc(inline)]
        pub use super::support::{DecodeError, Error, ResponseValue, Unset};

        /// A client for this API.
        ///
        /// Authentication and middleware are the host application's business: hand in a
        /// `reqwest::Client` configured however the service needs, and every request goes through
        /// it. progeny generates no hook system, because a preconfigured client already is one.
        #[derive(Debug, Clone)]
        pub struct Client {
            base_url: ::std::string::String,
            inner: ::reqwest::Client,
        }

        impl Client {
            /// A client against `base_url`, with a default `reqwest::Client`.
            #[must_use]
            pub fn new(base_url: impl ::std::convert::Into<::std::string::String>) -> Self {
                Self::with_client(base_url, ::reqwest::Client::new())
            }

            /// A client against `base_url`, using a `reqwest::Client` the caller configured.
            #[must_use]
            pub fn with_client(
                base_url: impl ::std::convert::Into<::std::string::String>,
                inner: ::reqwest::Client,
            ) -> Self {
                let mut base_url = base_url.into();
                // Joining is `format!("{base}{path}")` and every path starts with `/`, so a
                // trailing slash here would produce a double one on every request.
                while base_url.ends_with('/') {
                    base_url.pop();
                }
                Self { base_url, inner }
            }

            /// The base URL every request is built against.
            #[must_use]
            pub fn base_url(&self) -> &str {
                &self.base_url
            }

            /// The underlying HTTP client.
            #[must_use]
            pub fn client(&self) -> &::reqwest::Client {
                &self.inner
            }

            #(#methods)*
        }

        #base

        #(#builders)*
    }
}

/// A `Default` impl, when the document declares a server to default to.
///
/// Which URLs are usable is the model's ruling ([`ApiModel::default_server_url`]); this only
/// spells the impl.
fn default_base_url(model: &ApiModel) -> TokenStream {
    let Some(url) = model.default_server_url() else {
        return TokenStream::new();
    };
    let docs = format!(" The first server the description declares: `{url}`.");
    quote! {
        impl ::std::default::Default for Client {
            #[doc = #docs]
            fn default() -> Self {
                Self::new(#url)
            }
        }
    }
}

/// One operation's builder, its response types, and its `send`.
fn builder(operation: &OperationContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let name = builder_name(operation);
    let docs = docs_of(&operation.docs);
    let operation_name = operation.rust_name.as_str();

    let fields = operation.params.iter().map(|param| {
        let field = ident(&param.rust_name);
        let ty = type_tokens(&param.ty, contracts, config);
        quote! { #field: ::std::option::Option<#ty>, }
    });
    let body_field = operation.body.as_ref().map(|body| {
        let ty = body_type(body, contracts, config);
        quote! { body: ::std::option::Option<#ty>, }
    });
    let field_inits = operation.params.iter().map(|param| {
        let field = ident(&param.rust_name);
        quote! { #field: ::std::option::Option::None, }
    });
    let body_init = operation
        .body
        .as_ref()
        .map(|_| quote! { body: ::std::option::Option::None, });

    let setters = setters(operation, contracts, config);

    let success = success_type(operation, contracts, config);
    let failure = error_type(operation, contracts, config);
    let send = send(operation, &success.name, &failure.name, operation_name);
    let stream = stream(operation, contracts, config);

    let success_decl = &success.declaration;
    let failure_decl = &failure.declaration;
    let struct_docs = format!(
        " The request builder for [`Client::{operation_name}`]: `{} {}`.",
        operation.method.wire(),
        operation.path
    );
    // Two ways a builder names a deprecated thing, and it needs the allowance for either. Its own
    // `impl` uses the deprecated *builder* when the operation is deprecated — and an `impl` cannot
    // itself be deprecated, so the allowance is the only way to say "this is the deprecated thing,
    // not a use of one". And a parameter, body or response may be a deprecated *schema* type
    // whatever the operation's own status: `cloudflare` has a live operation with a deprecated
    // `feedback` parameter, which put the type in a field and in a setter, twice.
    let deprecation = (operation.docs.deprecated
        || !super::types::deprecated_use(operation_types(operation), contracts).is_empty())
    .then(|| quote! { #[allow(deprecated)] });

    quote! {
        #success_decl
        #failure_decl

        #docs
        #[doc = ""]
        #[doc = #struct_docs]
        #[derive(Debug, Clone)]
        #deprecation
        pub struct #name<'a> {
            client: &'a Client,
            #(#fields)*
            #body_field
        }

        #deprecation
        impl<'a> #name<'a> {
            fn new(client: &'a Client) -> Self {
                Self {
                    client,
                    #(#field_inits)*
                    #body_init
                }
            }

            #setters
            #send
            #stream
        }
    }
}

/// The stream over every item of every page, for an operation whose pagination was declared.
///
/// Built on the builder's own `send`, and on the builder deriving `Clone`: each page is a fresh
/// request with one parameter changed, which is exactly what the declaration describes. Nothing
/// here is inferred — the cursor parameter, the member holding the next cursor and the member
/// holding the items were all named by the caller and checked against the document before this ran.
///
/// The plain `send` stays. A stream is an additional way to call the operation and never the only
/// one: a caller who wants one page asks for one page.
fn stream(operation: &OperationContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let Some(pagination) = &operation.pagination else {
        return TokenStream::new();
    };
    let item = type_tokens(&pagination.item, contracts, config);
    let failure = error_type(operation, contracts, config).name;
    let cursor = ident(&pagination.cursor_param);
    let items_path = pagination.items.iter().map(ident);
    let next_path = pagination.next_cursor.iter().map(ident);
    let docs = format!(
        " Every item of every page, following `{}` until the service stops sending one.",
        pagination.cursor_param
    );

    quote! {
        #[doc = #docs]
        pub fn stream(
            self,
        ) -> impl ::futures_core::Stream<
            Item = ::std::result::Result<#item, Error<#failure>>,
        > + 'a {
            // `try_unfold` carries the builder forward as its state, so the borrow of the client
            // lives as long as the stream rather than as long as one page.
            let pages = ::futures_util::stream::try_unfold(
                ::std::option::Option::Some((self, ::std::option::Option::None)),
                |state| async move {
                    let ::std::option::Option::Some((builder, cursor)) = state else {
                        // The error type is named here and nowhere else in the closure: this arm
                        // never fails, so nothing else in it tells the compiler what `E` is.
                        return ::std::result::Result::<_, Error<#failure>>::Ok(
                            ::std::option::Option::None,
                        );
                    };
                    let mut request = ::std::clone::Clone::clone(&builder);
                    if let ::std::option::Option::Some(cursor) = cursor {
                        request.#cursor = ::std::option::Option::Some(cursor);
                    }
                    let page = request.send().await?.into_value();
                    // Cloned before the items are moved out of the same value, and in this order
                    // so that a next cursor living beside the items still reads.
                    let next = ::std::clone::Clone::clone(&page #(.#next_path)*);
                    let items = page #(.#items_path)*;
                    // The end of the stream is the service declining to send a next cursor, which
                    // is the only signal the declaration gives and the only one this trusts. A page
                    // that came back empty is not the same statement and does not stop it.
                    let state = next.map(|next| (builder, ::std::option::Option::Some(next)));
                    ::std::result::Result::Ok(::std::option::Option::Some((items, state)))
                },
            );
            ::futures_util::TryStreamExt::try_flatten(
                ::futures_util::TryStreamExt::map_ok(pages, |items| {
                    ::futures_util::stream::iter(
                        ::std::iter::IntoIterator::into_iter(items)
                            .map(::std::result::Result::Ok),
                    )
                }),
            )
        }
    }
}

/// One setter per parameter, and one for the body when the operation takes one.
///
/// Each takes `impl Into<T>` rather than `T`, so a caller writes `.name("x")` for a `String`.
fn setters(operation: &OperationContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let params = operation.params.iter().map(|param| {
        let field = ident(&param.rust_name);
        let ty = type_tokens(&param.ty, contracts, config);
        let docs = docs_of(&param.docs);
        let requirement = if param.required {
            format!(
                " Required. Sent as the `{}` {} parameter.",
                param.wire_name,
                param.style.location().slug()
            )
        } else {
            format!(
                " Sent as the `{}` {} parameter.",
                param.wire_name,
                param.style.location().slug()
            )
        };
        // The blank line is load-bearing. This sentence is progeny's, appended after the vendor's
        // prose, and without a paragraph break markdown reads it as a continuation of whatever the
        // description ended inside — `okta` ends several in a list item, which made progeny's own
        // sentence a lazy continuation of it. The struct docs and a documented `default` already
        // separate themselves the same way.
        quote! {
            #docs
            #[doc = ""]
            #[doc = #requirement]
            #[must_use]
            pub fn #field(mut self, value: impl ::std::convert::Into<#ty>) -> Self {
                self.#field = ::std::option::Option::Some(value.into());
                self
            }
        }
    });
    let body = operation.body.as_ref().map(|body| {
        let ty = body_type(body, contracts, config);
        let note = if body.required() {
            " The request body. Required."
        } else {
            " The request body."
        };
        quote! {
            #[doc = #note]
            #[must_use]
            pub fn body(mut self, value: impl ::std::convert::Into<#ty>) -> Self {
                self.body = ::std::option::Option::Some(value.into());
                self
            }
        }
    });
    quote! {
        #(#params)*
        #body
    }
}

/// A per-operation response type, and the tokens declaring it when one is needed.
struct Rendered {
    name: TokenStream,
    declaration: TokenStream,
}

/// Every type an operation names, for the questions that have to be asked of all of them at once.
pub(super) fn operation_types(operation: &OperationContract) -> Vec<&crate::contract::TypeRef> {
    let mut out: Vec<&crate::contract::TypeRef> =
        operation.params.iter().map(|param| &param.ty).collect();
    // Every body that has a type, not only the JSON one: a form or multipart body names the same
    // kind of generated type, and a deprecated one there warns exactly as loudly.
    out.extend(operation.body.as_ref().and_then(BodyContract::ty));
    out.extend(
        operation
            .responses
            .arms
            .iter()
            .chain(&operation.responses.default)
            .map(|arm| &arm.ty),
    );
    out
}

/// What a successful call yields.
fn success_type(operation: &OperationContract, contracts: &Contracts, config: &Config) -> Rendered {
    let arms: Vec<&ResponseArm> = success_arms(operation);
    match arms.as_slice() {
        // Nothing declared succeeds and no `default` catches it either. The call still has a
        // success path — a status the document never mentioned is `UnexpectedStatus` — and there is
        // nothing left to hand back but unit.
        [] => Rendered {
            name: quote! { () },
            declaration: TokenStream::new(),
        },
        [only] => Rendered {
            name: type_tokens(&only.ty, contracts, config),
            declaration: TokenStream::new(),
        },
        several => {
            let sized = super::types::variant_sizes();
            let name = format_ident!(
                "{}Success",
                operation
                    .rust_name
                    .as_str()
                    .split('_')
                    .map(capitalize)
                    .collect::<String>()
            );
            let variants = several.iter().map(|arm| {
                let variant = format_ident!("{}", arm.rust_name.as_str());
                let ty = type_tokens(&arm.ty, contracts, config);
                let docs = docs_of(&arm.docs);
                quote! { #docs #variant(#ty), }
            });
            let docs = format!(
                " The successful responses [`Client::{}`] declares.",
                operation.rust_name.as_str()
            );
            Rendered {
                name: quote! { #name },
                declaration: quote! {
                    #[doc = #docs]
                    #[derive(Debug, Clone)]
                    #sized
                    pub enum #name { #(#variants)* }
                },
            }
        }
    }
}

/// The arms a 2xx status can land in.
///
/// The `default` arm counts **only when the document declares no successful status at all**, which
/// is a shape real descriptions have: `default` means "anything not otherwise claimed", so with no
/// `2XX` beside it, a `200` is exactly that. Treating it as an error instead would report every
/// successful call as a failure. Where explicit success arms *do* exist, `default` stays a failure:
/// the document has already said what success looks like, and an undeclared success is a stranger.
fn success_arms(operation: &OperationContract) -> Vec<&ResponseArm> {
    let declared: Vec<&ResponseArm> = operation
        .responses
        .arms
        .iter()
        .filter(|arm| arm.status.is_success())
        .collect();
    if declared.is_empty() {
        return operation.responses.default.iter().collect();
    }
    declared
}

/// Whether the `default` arm is this operation's success as well as its failure.
fn default_is_success(operation: &OperationContract) -> bool {
    operation.responses.default.is_some()
        && !operation
            .responses
            .arms
            .iter()
            .any(|arm| arm.status.is_success())
}

/// What a declared failure carries.
fn error_type(operation: &OperationContract, contracts: &Contracts, config: &Config) -> Rendered {
    let arms: Vec<&ResponseArm> = operation
        .responses
        .arms
        .iter()
        .filter(|arm| !arm.status.is_success())
        .chain(&operation.responses.default)
        .collect();
    match arms.as_slice() {
        [] => Rendered {
            name: quote! { () },
            declaration: TokenStream::new(),
        },
        [only] => Rendered {
            name: type_tokens(&only.ty, contracts, config),
            declaration: TokenStream::new(),
        },
        several => {
            let sized = super::types::variant_sizes();
            let name = format_ident!(
                "{}Error",
                operation
                    .rust_name
                    .as_str()
                    .split('_')
                    .map(capitalize)
                    .collect::<String>()
            );
            let variants = several.iter().map(|arm| {
                let variant = format_ident!("{}", arm.rust_name.as_str());
                let ty = type_tokens(&arm.ty, contracts, config);
                let docs = docs_of(&arm.docs);
                quote! { #docs #variant(#ty), }
            });
            let docs = format!(
                " The failures [`Client::{}`] declares.",
                operation.rust_name.as_str()
            );
            Rendered {
                name: quote! { #name },
                declaration: quote! {
                    #[doc = #docs]
                    #[derive(Debug, Clone)]
                    #sized
                    pub enum #name { #(#variants)* }
                },
            }
        }
    }
}

/// The `send` method: build the URL, add the parameters, dispatch on the status.
fn send(
    operation: &OperationContract,
    success: &TokenStream,
    failure: &TokenStream,
    operation_name: &str,
) -> TokenStream {
    let method = format_ident!("{}", operation.method.wire());
    let path = path_expression(operation, operation_name);
    let query = query(operation);
    let headers = headers(operation);
    let body = request_body(operation, operation_name);
    let dispatch = dispatch(operation, success, failure);
    // `mut` only where something actually reassigns it: a generated crate is checked with warnings
    // denied, so an unnecessary `mut` is a defect a consumer sees rather than a wart nobody reads.
    let mutability =
        (!query.is_empty() || !headers.is_empty() || !body.is_empty()).then(|| quote! { mut });
    let missing_body = operation
        .body
        .as_ref()
        .filter(|body| body.required())
        .map(|_| {
            quote! {
                if self.body.is_none() {
                    return ::std::result::Result::Err(Error::Decode(
                        support::DecodeError::new(
                            ::reqwest::StatusCode::BAD_REQUEST,
                            ::serde::de::Error::custom(
                                support::Unset::new(#operation_name, "a request body"),
                            ),
                        ),
                    ));
                }
            }
        });

    quote! {
        /// Perform the request.
        ///
        /// # Errors
        ///
        /// Returns [`Error`] when the request fails, when the server answers with a status the
        /// description declares as an error, when it answers with one the description does not
        /// declare at all, or when a body does not match the shape the description states.
        pub async fn send(self) -> ::std::result::Result<ResponseValue<#success>, Error<#failure>> {
            #missing_body
            let url = #path;
            let #mutability request = self.client.client().request(::reqwest::Method::#method, url);
            #query
            #headers
            #body
            let response = request.send().await?;
            #dispatch
        }
    }
}

/// The URL expression: the base, then the template with its variables filled in.
fn path_expression(operation: &OperationContract, operation_name: &str) -> TokenStream {
    let mut steps = Vec::new();
    for segment in operation.path.segments() {
        steps.push(quote! { url.push('/'); });
        for piece in segment.pieces() {
            match piece {
                Piece::Literal(text) => {
                    let mut characters = text.chars();
                    match (characters.next(), characters.next()) {
                        (None, _) => {}
                        // `push_str` with a one-character literal is a lint the consumer sees, and
                        // a route like `stream.{container}` puts single characters between
                        // variables often enough for it to be worth the arm.
                        (Some(only), None) => steps.push(quote! { url.push(#only); }),
                        _ => steps.push(quote! { url.push_str(#text); }),
                    }
                }
                Piece::Variable(name) => {
                    let Some(param) = operation.params.iter().find(|param| {
                        param.wire_name == *name && param.style.location() == Location::Path
                    }) else {
                        // Unreachable: an unbound variable took its operation out of the model.
                        continue;
                    };
                    let field = ident(&param.rust_name);
                    let style = style_tokens(param.style.style());
                    let explode = param.style.explode();
                    let wire = &param.wire_name;
                    let rendered = if param.style.style() == Style::Matrix {
                        quote! { support::style::matrix_segment(#wire, &value, #explode) }
                    } else {
                        quote! { support::style::path_segment(&value, #style, #explode) }
                    };
                    steps.push(quote! {
                        {
                            let value = match &self.#field {
                                ::std::option::Option::Some(value) => ::serde_json::to_value(value)
                                    .unwrap_or(::serde_json::Value::Null),
                                ::std::option::Option::None => {
                                    return ::std::result::Result::Err(Error::Decode(
                                        support::DecodeError::new(
                                            ::reqwest::StatusCode::BAD_REQUEST,
                                            ::serde::de::Error::custom(
                                                support::Unset::new(#operation_name, #wire),
                                            ),
                                        ),
                                    ));
                                }
                            };
                            url.push_str(&#rendered);
                        }
                    });
                }
            }
        }
    }
    quote! {
        {
            let mut url = ::std::string::String::from(self.client.base_url());
            #(#steps)*
            url
        }
    }
}

fn query(operation: &OperationContract) -> TokenStream {
    let pairs: Vec<TokenStream> = operation
        .params_at(Location::Query)
        .map(|param| {
            let field = ident(&param.rust_name);
            let wire = &param.wire_name;
            let style = style_tokens(param.style.style());
            let explode = param.style.explode();
            let body = quote! {
                let value = ::serde_json::to_value(value).unwrap_or(::serde_json::Value::Null);
                query.extend(support::style::query_pairs(#wire, &value, #style, #explode));
            };
            optional(param, operation, &field, &body)
        })
        .collect();
    if pairs.is_empty() {
        return TokenStream::new();
    }
    quote! {
        let mut query: ::std::vec::Vec<(::std::string::String, ::std::string::String)> =
            ::std::vec::Vec::new();
        #(#pairs)*
        if !query.is_empty() {
            request = request.query(&query);
        }
    }
}

fn headers(operation: &OperationContract) -> TokenStream {
    let mut pieces = Vec::new();
    for param in operation.params_at(Location::Header) {
        let field = ident(&param.rust_name);
        let wire = &param.wire_name;
        let explode = param.style.explode();
        let body = quote! {
            let value = ::serde_json::to_value(value).unwrap_or(::serde_json::Value::Null);
            request = request.header(#wire, support::style::header_value(&value, #explode));
        };
        pieces.push(optional(param, operation, &field, &body));
    }

    let cookies: Vec<TokenStream> = operation
        .params_at(Location::Cookie)
        .map(|param| {
            let field = ident(&param.rust_name);
            let wire = &param.wire_name;
            let body = quote! {
                let value = ::serde_json::to_value(value).unwrap_or(::serde_json::Value::Null);
                cookies.push(support::style::cookie_pair(#wire, &value));
            };
            optional(param, operation, &field, &body)
        })
        .collect();
    if !cookies.is_empty() {
        // One `Cookie` header carrying every cookie parameter, which is how a cookie header is
        // written; one header per parameter would be a different request.
        pieces.push(quote! {
            let mut cookies: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
            #(#cookies)*
            if !cookies.is_empty() {
                request = request.header("cookie", cookies.join("; "));
            }
        });
    }
    quote! { #(#pieces)* }
}

/// Read a parameter that may not be set, and do `body` with it.
///
/// An `if let` where an unset value simply contributes nothing, and a `match` only where the
/// document said the parameter is required and there is a refusal to run. A `match` whose second
/// arm is empty is a lint the consumer of a generated crate sees.
fn optional(
    param: &ParamContract,
    operation: &OperationContract,
    field: &proc_macro2::Ident,
    body: &TokenStream,
) -> TokenStream {
    let required = required_guard(param, operation);
    if required.is_empty() {
        return quote! {
            if let ::std::option::Option::Some(value) = &self.#field {
                #body
            }
        };
    }
    quote! {
        match &self.#field {
            ::std::option::Option::Some(value) => { #body }
            ::std::option::Option::None => { #required }
        }
    }
}

/// What an unset parameter does, which is nothing unless the document said it is required.
fn required_guard(param: &ParamContract, operation: &OperationContract) -> TokenStream {
    if !param.required {
        return TokenStream::new();
    }
    let operation_name = operation.rust_name.as_str();
    let wire = &param.wire_name;
    quote! {
        return ::std::result::Result::Err(Error::Decode(support::DecodeError::new(
            ::reqwest::StatusCode::BAD_REQUEST,
            ::serde::de::Error::custom(support::Unset::new(#operation_name, #wire)),
        )));
    }
}

fn request_body(operation: &OperationContract, operation_name: &str) -> TokenStream {
    let inner = match &operation.body {
        None => return TokenStream::new(),
        Some(BodyContract::Json { .. }) => quote! { request = request.json(body); },
        Some(BodyContract::Form { specs, .. }) => {
            let specs = form_specs(specs);
            quote! {
                let value = support::to_value(body, #operation_name).map_err(Error::Decode)?;
                request = request
                    .header(
                        ::reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(support::style::form_body(&value, #specs));
            }
        }
        Some(BodyContract::Multipart { parts, .. }) => {
            let parts = part_specs(parts);
            // A multipart body whose value is not an object has no member names, so there is
            // nothing to name the parts after. That is a body the document typed as something
            // other than an object, and reporting it at send time is the only place it can be
            // reported: the type is legal Rust and the failure is about this one call.
            quote! {
                let value = support::to_value(body, #operation_name).map_err(Error::Decode)?;
                let (content_type, bytes) =
                    support::multipart::body(&value, #parts).ok_or_else(|| {
                        Error::Decode(support::DecodeError::new(
                            ::reqwest::StatusCode::BAD_REQUEST,
                            ::serde::de::Error::custom(support::NotAForm::new(#operation_name)),
                        ))
                    })?;
                request = request
                    .header(::reqwest::header::CONTENT_TYPE, content_type)
                    .body(bytes);
            }
        }
        // Text and bytes write the same statement; what differs is the *type* the builder holds,
        // which `body_type` decides. `reqwest::Body` takes both a `String` and a `Vec<u8>`.
        Some(
            BodyContract::Text { content_type, .. } | BodyContract::Bytes { content_type, .. },
        ) => quote! {
            request = request
                .header(::reqwest::header::CONTENT_TYPE, #content_type)
                .body(body.clone());
        },
    };
    quote! {
        if let ::std::option::Option::Some(body) = &self.body {
            #inner
        }
    }
}

/// The part table for a multipart body, as a `const` slice.
///
/// A table rather than a call per part: the loop lives in the shipped support module and is
/// compiled once for the crate, instead of being unrolled into every operation that sends a form.
pub(super) fn part_specs(parts: &[PartSpec]) -> TokenStream {
    let rows = parts.iter().map(|part| {
        let name = &part.wire_name;
        let kind = format_ident!(
            "{}",
            match part.kind {
                PartKind::Text => "Text",
                PartKind::File => "File",
                PartKind::Json => "Json",
            }
        );
        let repeated = part.repeated;
        let content_type = part.content_type.as_ref().map_or_else(
            || quote! { ::std::option::Option::None },
            |content_type| quote! { ::std::option::Option::Some(#content_type) },
        );
        quote! {
            support::multipart::PartSpec {
                name: #name,
                kind: support::multipart::PartKind::#kind,
                repeated: #repeated,
                content_type: #content_type,
            }
        }
    });
    quote! { &[#(#rows),*] }
}

/// The same, for the members of a form body whose `encoding` said something about them.
pub(super) fn form_specs(specs: &[FormSpec]) -> TokenStream {
    let rows = specs.iter().map(|spec| {
        let name = &spec.wire_name;
        let style = style_tokens(spec.style);
        let explode = spec.explode;
        let array = spec.array.map_or_else(
            || quote! { ::std::option::Option::None },
            |array| quote! { ::std::option::Option::Some(#array) },
        );
        quote! {
            support::style::FormSpec { name: #name, style: #style, explode: #explode, array: #array }
        }
    });
    quote! { &[#(#rows),*] }
}

/// Turn the response into the operation's declared arms.
fn dispatch(
    operation: &OperationContract,
    success: &TokenStream,
    failure: &TokenStream,
) -> TokenStream {
    let successes: Vec<&ResponseArm> = operation
        .responses
        .arms
        .iter()
        .filter(|arm| arm.status.is_success())
        .collect();
    let failures: Vec<&ResponseArm> = operation
        .responses
        .arms
        .iter()
        .filter(|arm| !arm.status.is_success())
        .collect();
    let claims_success = default_is_success(operation);
    // Counted exactly as [`error_type`] counts them, because the two answers have to be the same
    // one: it declares the enum, this decides whether to put a body in a variant of it. They were
    // allowed to disagree, and the case that told them apart is a document with a non-2xx arm and a
    // `default` that claims success — `weather-gov` writes a `302` and a `default` and no `2XX`.
    // `error_type` saw two arms and declared the enum; this saw one and left the body unwrapped, so
    // the generated code asked the *enum* to deserialize. It derives `Debug, Clone` and nothing
    // else, so the client did not compile at all. A `default` is a failure arm whenever it exists —
    // claiming success as well does not stop `_ =>` from decoding through it.
    let wrapped_failure = failures.len() + usize::from(operation.responses.default.is_some()) > 1;

    let success_arms = arm_matches(&successes, success, successes.len() > 1, false);
    let failure_arms = arm_matches(&failures, failure, wrapped_failure, true);
    let fallback = match (&operation.responses.default, claims_success) {
        // No `default`: a status the document never mentioned has no shape progeny has ever seen.
        (None, _) => quote! { _ => ::std::result::Result::Err(Error::UnexpectedStatus(response)), },
        // A `default` beside declared successes catches what they do not, which is a failure.
        (Some(arm), false) => {
            let decoded = decode(arm, failure, wrapped_failure);
            quote! { _ => ::std::result::Result::Err(Error::Declared(#decoded)), }
        }
        // A `default` and nothing else: it is the whole contract, so a 2xx through it succeeded.
        (Some(arm), true) => {
            let ok = decode(arm, success, false);
            let err = decode(arm, failure, wrapped_failure);
            quote! {
                200..=299 => ::std::result::Result::Ok(#ok),
                _ => ::std::result::Result::Err(Error::Declared(#err)),
            }
        }
    };

    quote! {
        let status = response.status().as_u16();
        // The overlap clippy would report here *is* the contract: OpenAPI says an exact status
        // claims a response before a range does, so a document declaring both `400` and `4XX` gets
        // arms that overlap by construction, ordered exact-first by `StatusPattern::precedence`.
        // The lint reads that as a mistake, which is the one case suppression is for.
        #[allow(clippy::match_overlapping_arm)]
        match status {
            #(#success_arms)*
            #(#failure_arms)*
            #fallback
        }
    }
}

/// One `match` arm per declared status.
fn arm_matches(
    arms: &[&ResponseArm],
    ty: &TokenStream,
    wrapped: bool,
    is_error: bool,
) -> Vec<TokenStream> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for arm in arms {
        let pattern = match arm.status {
            crate::api::StatusPattern::Exact(code) => {
                if !seen.insert(u32::from(code)) {
                    continue;
                }
                quote! { #code }
            }
            crate::api::StatusPattern::Range(hundreds) => {
                let low = u16::from(hundreds) * 100;
                let high = low + 99;
                quote! { #low..=#high }
            }
        };
        let decoded = decode(arm, ty, wrapped);
        out.push(if is_error {
            quote! {
                #pattern => ::std::result::Result::Err(Error::Declared(#decoded)),
            }
        } else {
            quote! { #pattern => ::std::result::Result::Ok(#decoded), }
        });
    }
    out
}

/// Parse the body and put it in its variant, if the operation needed variants.
///
/// Written as a `map` over the response rather than as a `let` and a rebuild, so the body is never
/// bound to a name: a `204` arm's body is `()`, and binding a unit is a lint the consumer sees.
fn decode(arm: &ResponseArm, ty: &TokenStream, wrapped: bool) -> TokenStream {
    if !wrapped {
        return quote! { support::decode(response).await? };
    }
    let variant = format_ident!("{}", arm.rust_name.as_str());
    quote! { support::decode(response).await?.map(#ty::#variant) }
}

pub(crate) fn body_type(
    body: &BodyContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    match body {
        BodyContract::Json { ty, .. }
        | BodyContract::Form { ty, .. }
        | BodyContract::Multipart { ty, .. } => type_tokens(ty, contracts, config),
        // Text rather than bytes, because that is what the caller has. The one place the byte
        // representation is a choice is a body the document said nothing about the shape of.
        BodyContract::Text { .. } => quote! { ::std::string::String },
        BodyContract::Bytes { .. } => match config.formats.bytes {
            BytesRepr::Vec => quote! { ::std::vec::Vec<u8> },
            BytesRepr::Bytes => quote! { ::bytes::Bytes },
        },
    }
}

pub(super) fn style_tokens(style: Style) -> TokenStream {
    let name = match style {
        Style::Form => "Form",
        Style::Simple => "Simple",
        Style::Label => "Label",
        Style::Matrix => "Matrix",
        Style::SpaceDelimited => "SpaceDelimited",
        Style::PipeDelimited => "PipeDelimited",
        Style::DeepObject => "DeepObject",
    };
    let ident = format_ident!("{name}");
    quote! { support::style::Style::#ident }
}

fn builder_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!(
        "{}",
        operation
            .rust_name
            .as_str()
            .split('_')
            .map(capitalize)
            .collect::<String>()
    )
}

pub(super) fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}
