//! The calling side: one `Client`, one params struct and one request per operation.
//!
//! Everything here is transcription. Which media type a body uses, how a parameter serializes,
//! which status arms exist and in what order — all of it was decided in [`crate::api`] and is
//! sitting in a record; this turns records into tokens and makes no choice of its own.
//!
//! Two shapes are worth knowing before reading the output.
//!
//! **Parameters are a struct, checked by construction.** An operation that takes input gets a
//! `FooParams` struct — a required input is a plain field, an optional one an `Option`, and the
//! struct literal is the only constructor — so a missing required input, or one the description
//! adds later, is a missing-field compile error at every call site rather than anything at
//! runtime. The `FooRequest` the accessor returns binds those params to the client and carries
//! only what the description does not declare — an extra header — and `send`.
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
    PartSpec, Piece, ResponseArm, ResponseBody, Style,
};
use crate::config::{BytesRepr, Config};
use crate::contract::{Contracts, RustIdent, TypeRef};

use super::types::{
    docs as docs_of, docs_prose, response_body_is_boxed, response_enum_type_path,
    response_type_path, type_path as type_tokens,
};

/// Render the client module.
pub(super) fn render(model: &ApiModel, contracts: &Contracts, config: &Config) -> TokenStream {
    let base = default_base_url(model);
    let methods = model.operations().iter().map(|operation| {
        let name = ident(&operation.rust_name);
        let request = request_name(operation);
        let docs = docs_of(&operation.docs);
        // A deprecated operation's request and params are `#[deprecated]`, and this accessor names
        // both — uses rustc lints, in the consumer's crate, about code the consumer did not
        // write. The method carrying `#[deprecated]` itself exempts neither: being deprecated is
        // not a licence to name deprecated things. The request's own `impl` carries the same
        // expectation for the same reason.
        let deprecation = operation.docs.deprecated.then(|| {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the generated accessor deliberately returns a deprecated operation"
                )]
            }
        });
        let arm = if takes_params(operation) {
            let params = params_name(operation);
            quote! {
                pub fn #name(&self, params: #params) -> #request<'_> {
                    #request::new(self, params)
                }
            }
        } else {
            quote! {
                pub fn #name(&self) -> #request<'_> {
                    #request::new(self)
                }
            }
        };
        quote! {
            #docs
            #deprecation
            #[must_use]
            #arm
        }
    });

    let interfaces = model
        .operations()
        .iter()
        .map(|operation| interface(operation, contracts, config));

    quote! {
        //! The calling side.

        use super::support;

        #[doc(inline)]
        pub use super::support::{DecodeError, Error, ResponseValue};

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

        #(#interfaces)*
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

/// One operation's params struct, its request, its response types, and its `send`.
fn interface(operation: &OperationContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let request = request_name(operation);
    let docs = docs_of(&operation.docs);
    let operation_name = operation.rust_name.as_str();

    let takes = takes_params(operation);
    let params = params_name(operation);
    let params_decl = takes.then(|| params_struct(operation, contracts, config));
    let params_field = takes.then(|| {
        // The params type is `#[deprecated]` when the operation is, and a field naming it lints
        // even inside a struct that is itself deprecated — `#[deprecated]` on the holder exempts
        // nothing. Scoped to the one field that must name the type.
        let exempt = operation.docs.deprecated.then(|| {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the generated request deliberately exposes deprecated API"
                )]
            }
        });
        quote! {
            #exempt
            params: #params,
        }
    });
    let params_arg = takes.then(|| quote! { , params: #params });
    let params_init = takes.then(|| quote! { params, });

    let success = success_type(operation, contracts, config);
    let failure = error_type(operation, contracts, config);
    let send = send(
        operation,
        &success.name,
        &failure.name,
        operation_name,
        config,
    );
    let stream = stream(operation, contracts, config);

    let success_decl = &success.declaration;
    let failure_decl = &failure.declaration;
    let struct_docs = format!(
        " The prepared request for [`Client::{operation_name}`]: `{} {}`.",
        operation.method.wire(),
        operation.path
    );
    let holds = if takes {
        format!(
            " Holds the client and the operation's [`{params}`]. Everything the description \
             declares lives on the params; what it does not — an extra header — is set here."
        )
    } else {
        " Holds the client. A header the description does not declare is set here.".to_owned()
    };
    // An `impl` uses its deprecated request when the operation is deprecated, but an `impl` cannot
    // itself be deprecated. The expectation says this is the implementation of that deprecated
    // thing, not a new call site. Deprecated schema types are handled at the exact fields and
    // methods that name them.
    let deprecation = operation.docs.deprecated.then(|| {
        quote! {
            #[expect(
                deprecated,
                reason = "the generated request deliberately exposes deprecated API"
            )]
        }
    });

    quote! {
        #success_decl
        #failure_decl

        #params_decl

        #docs
        #[doc = ""]
        #[doc = #struct_docs]
        #[doc = ""]
        #[doc = #holds]
        #[derive(Debug, Clone)]
        pub struct #request<'a> {
            client: &'a Client,
            #params_field
            headers: ::reqwest::header::HeaderMap,
        }

        #deprecation
        impl<'a> #request<'a> {
            fn new(client: &'a Client #params_arg) -> Self {
                Self {
                    client,
                    #params_init
                    headers: ::reqwest::header::HeaderMap::new(),
                }
            }

            /// Set a header the description does not declare.
            ///
            /// A header the operation declares is a field of the params and belongs there; one
            /// set here replaces everything the request assembled under the same name — a
            /// declared header, the cookies, the body's content type. Typed as reqwest's own
            /// name and value so an invalid header cannot exist to fail at send time.
            #[must_use]
            pub fn header(
                mut self,
                name: ::reqwest::header::HeaderName,
                value: ::reqwest::header::HeaderValue,
            ) -> Self {
                self.headers.append(name, value);
                self
            }

            #send
            #stream
        }
    }
}

/// The params struct: every input the operation takes, as one caller-built literal.
///
/// A required input is a plain field and an optional one an `Option`; there is no constructor and
/// no `Default`, so the literal is the only way to build one and every call site names every
/// input. A parameter the description adds later is a missing-field compile error at each of them
/// rather than a request quietly sent without it.
fn params_struct(
    operation: &OperationContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = params_name(operation);
    let operation_name = operation.rust_name.as_str();
    let docs = docs_of(&operation.docs);
    let fields = operation.params.iter().map(|param| {
        let field = ident(&param.rust_name);
        let ty = type_tokens(&param.ty, contracts, config);
        let ty = if param.required {
            ty
        } else {
            quote! { ::std::option::Option<#ty> }
        };
        let docs = docs_prose(&param.docs);
        let note = requirement_note(param);
        // The blank line is load-bearing. This sentence is progeny's, appended after the vendor's
        // prose, and without a paragraph break markdown reads it as a continuation of whatever the
        // description ended inside — `okta` ends several in a list item, which made progeny's own
        // sentence a lazy continuation of it. The struct docs and a documented `default` already
        // separate themselves the same way.
        quote! {
            #docs
            #[doc = ""]
            #[doc = #note]
            pub #field: #ty,
        }
    });
    let body_field = operation.body.as_ref().map(|body| {
        let ty = body_type(body, contracts, config);
        let (ty, note) = if body.required() {
            (ty, " The request body. Required.")
        } else {
            (quote! { ::std::option::Option<#ty> }, " The request body.")
        };
        quote! {
            #[doc = #note]
            pub body: #ty,
        }
    });
    let struct_docs = format!(
        " The parameters of [`Client::{operation_name}`]: `{} {}`.",
        operation.method.wire(),
        operation.path
    );
    quote! {
        #docs
        #[doc = ""]
        #[doc = #struct_docs]
        #[doc = ""]
        #[doc = " Every input the operation takes: a required one as a plain field, an optional \
                 one as an `Option`. The struct literal is the only constructor, so a call site \
                 names them all — an input the description adds later is a compile error here \
                 rather than a request quietly sent without it."]
        #[derive(Debug, Clone)]
        pub struct #name {
            #(#fields)*
            #body_field
        }
    }
}

/// The provenance line under an input's own docs: where it travels, whether it must be set, and
/// whether the document deprecates it — in prose, because the field cannot carry
/// `#[deprecated]` without warning at every literal that is obliged to name it.
fn requirement_note(param: &ParamContract) -> String {
    let deprecated = if param.docs.deprecated {
        "Deprecated. "
    } else {
        ""
    };
    if param.required {
        format!(
            " {deprecated}Required. Sent as the `{}` {} parameter.",
            param.wire_name,
            param.style.location().slug()
        )
    } else {
        format!(
            " {deprecated}Sent as the `{}` {} parameter.",
            param.wire_name,
            param.style.location().slug()
        )
    }
}

/// The stream over every item of every page, for an operation whose pagination was declared.
///
/// Built on the request's own `send`, and on the request deriving `Clone`: each page is a fresh
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
    // The next cursor is bound bare — the page member is an `Option` by [`attach`]'s own rule,
    // and the `map` peels it — while the params field wraps the parameter's type in one
    // `Option` per fact that makes it optional: an omitted `required`, and a nullable type of
    // its own. The assignment restores exactly those layers.
    let layers = operation
        .params
        .iter()
        .find(|param| param.rust_name == pagination.cursor_param)
        .map_or(1, |param| {
            usize::from(!param.required) + usize::from(matches!(param.ty, TypeRef::Option(_)))
        });
    let mut value = quote! { cursor };
    for _ in 0..layers {
        value = quote! { ::std::option::Option::Some(#value) };
    }
    let assign = quote! { page_request.params.#cursor = #value; };
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
            // `try_unfold` carries the request forward as its state, so the borrow of the client
            // lives as long as the stream rather than as long as one page.
            let pages = ::futures_util::stream::try_unfold(
                ::std::option::Option::Some((self, ::std::option::Option::None)),
                |state| async move {
                    let ::std::option::Option::Some((request, cursor)) = state else {
                        // The error type is named here and nowhere else in the closure: this arm
                        // never fails, so nothing else in it tells the compiler what `E` is.
                        return ::std::result::Result::<_, Error<#failure>>::Ok(
                            ::std::option::Option::None,
                        );
                    };
                    let mut page_request = ::std::clone::Clone::clone(&request);
                    if let ::std::option::Option::Some(cursor) = cursor {
                        #assign
                    }
                    let page = page_request.send().await?.into_value();
                    // Cloned before the items are moved out of the same value, and in this order
                    // so that a next cursor living beside the items still reads.
                    let next = ::std::clone::Clone::clone(&page #(.#next_path)*);
                    let items = page #(.#items_path)*;
                    // The end of the stream is the service declining to send a next cursor, which
                    // is the only signal the declaration gives and the only one this trusts. A page
                    // that came back empty is not the same statement and does not stop it.
                    let state = next.map(|next| (request, ::std::option::Option::Some(next)));
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

/// A per-operation response type, and the tokens declaring it when one is needed.
struct Rendered {
    name: TokenStream,
    declaration: TokenStream,
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
            name: response_type_path(&only.body, contracts, config),
            declaration: TokenStream::new(),
        },
        several => {
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
                let ty = response_enum_type_path(&arm.body, contracts, config);
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
            name: response_type_path(&only.body, contracts, config),
            declaration: TokenStream::new(),
        },
        several => {
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
                let ty = response_enum_type_path(&arm.body, contracts, config);
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
    config: &Config,
) -> TokenStream {
    let method = format_ident!("{}", operation.method.wire());
    let path = path_expression(operation);
    let query = query(operation);
    let headers = headers(operation);
    let flag = needs_content_type_flag(operation)
        .then(|| quote! { let mut declared_content_type = false; });
    let accept = accept_header(operation);
    let body = request_body(operation, operation_name);
    let dispatch = dispatch(operation, success, failure, config);

    quote! {
        /// Perform the request.
        ///
        /// # Errors
        ///
        /// Returns [`Error`] when the request fails, when the server answers with a status the
        /// description declares as an error, when it answers with one the description does not
        /// declare at all, or when a body does not match the shape the description states.
        pub async fn send(self) -> ::std::result::Result<ResponseValue<#success>, Error<#failure>> {
            let url = #path;
            let mut request = self.client.client().request(::reqwest::Method::#method, url);
            #flag
            #query
            #headers
            #accept
            #body
            if !self.headers.is_empty() {
                // Applied last, and reqwest replaces same-named headers here: a header set on
                // the request wins over everything the operation assembled for that name — a
                // declared parameter, the cookies, the body's content type.
                request = request.headers(self.headers);
            }
            let response = request.send().await?;
            #dispatch
        }
    }
}

/// The URL expression: the base, then the template with its variables filled in.
fn path_expression(operation: &OperationContract) -> TokenStream {
    let mut steps = Vec::new();
    for segment in operation.path.segments() {
        let variable_names: Vec<&str> = segment
            .pieces()
            .iter()
            .filter_map(|piece| match piece {
                Piece::Variable(name) => Some(name.as_str()),
                Piece::Literal(_) => None,
            })
            .collect();
        if variable_names.is_empty() {
            steps.push(quote! { url.push('/'); });
            for piece in segment.pieces() {
                let Piece::Literal(text) = piece else {
                    continue;
                };
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
            continue;
        }
        // A segment with a variable is assembled apart and checked before it joins the URL: a
        // caller value of `.` or `..` renders a dot segment, which every WHATWG parser —
        // reqwest's included — folds away before the request leaves, so `/files/{id}/meta`
        // with `id = ".."` would silently ask for `/meta`. Percent-encoding cannot save it
        // (`%2E` segments normalize the same way), so the request is refused instead.
        let mut piece_steps = Vec::new();
        for piece in segment.pieces() {
            match piece {
                Piece::Literal(text) => {
                    let mut characters = text.chars();
                    match (characters.next(), characters.next()) {
                        (None, _) => {}
                        (Some(only), None) => piece_steps.push(quote! { segment.push(#only); }),
                        _ => piece_steps.push(quote! { segment.push_str(#text); }),
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
                    // Always a direct read: the contract records a path parameter as required
                    // whatever the document says, so the field is never an `Option`. Bound
                    // before serializing so the call receives a place of reference type — a
                    // literal `&` on a `Copy` field is a `needless_borrow` lint in the
                    // consumer's build.
                    piece_steps.push(quote! {
                        {
                            let value = &self.params.#field;
                            let value = ::serde_json::to_value(value)
                                .unwrap_or(::serde_json::Value::Null);
                            segment.push_str(&#rendered);
                        }
                    });
                }
            }
        }
        let blame = variable_names.first().copied().unwrap_or_default();
        steps.push(quote! {
            {
                let mut segment = ::std::string::String::new();
                #(#piece_steps)*
                if matches!(segment.as_str(), "." | "..") {
                    return ::std::result::Result::Err(Error::UnsendablePath {
                        parameter: #blame,
                        rendered: segment,
                    });
                }
                url.push('/');
                url.push_str(&segment);
            }
        });
    }
    let reserved = reserved_query(operation);
    quote! {
        {
            let mut url = ::std::string::String::from(self.client.base_url());
            #(#steps)*
            #reserved
            url
        }
    }
}

/// The query pairs `allowReserved: true` routes around reqwest's encoder.
///
/// reqwest's `.query(...)` percent-encodes every reserved character in the values it is handed,
/// which is exactly the spelling the flag turns off. These pairs are appended to the URL string
/// directly, through [`support::style::reserved_pair`]'s reserved-preserving encoding; pairs
/// reqwest adds afterwards join the query string behind them correctly.
fn reserved_query(operation: &OperationContract) -> TokenStream {
    let pairs: Vec<TokenStream> = operation
        .params_at(Location::Query)
        .filter(|param| param.style.allow_reserved())
        .map(|param| {
            let field = ident(&param.rust_name);
            let wire = &param.wire_name;
            let style = style_tokens(param.style.style());
            let explode = param.style.explode();
            let body = quote! {
                let value = ::serde_json::to_value(value).unwrap_or(::serde_json::Value::Null);
                reserved.extend(support::style::query_pairs(#wire, &value, #style, #explode));
            };
            read_param(param, &field, &body)
        })
        .collect();
    if pairs.is_empty() {
        return TokenStream::new();
    }
    quote! {
        let mut reserved: ::std::vec::Vec<(::std::string::String, ::std::string::String)> =
            ::std::vec::Vec::new();
        #(#pairs)*
        for (index, (name, value)) in reserved.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(&support::style::reserved_pair(name, value));
        }
    }
}

fn query(operation: &OperationContract) -> TokenStream {
    let pairs: Vec<TokenStream> = operation
        .params_at(Location::Query)
        .filter(|param| !param.style.allow_reserved())
        .map(|param| {
            let field = ident(&param.rust_name);
            let wire = &param.wire_name;
            let style = style_tokens(param.style.style());
            let explode = param.style.explode();
            let body = quote! {
                let value = ::serde_json::to_value(value).unwrap_or(::serde_json::Value::Null);
                query.extend(support::style::query_pairs(#wire, &value, #style, #explode));
            };
            read_param(param, &field, &body)
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
        // The flag `body_content_type` reads: a set `Content-Type` parameter means the body's
        // automatic header stays home.
        let mark = (needs_content_type_flag(operation)
            && param.wire_name.eq_ignore_ascii_case("content-type"))
        .then(|| quote! { declared_content_type = true; });
        let body = quote! {
            #body
            #mark
        };
        // A declared optional `Accept` parameter outranks the configured media types only when
        // the caller actually set it. Unset, the configured types still have to reach the wire
        // — suppressing them on the mere declaration left the server free to answer with the
        // type the configuration refused.
        let fallback = (!param.required
            && param.wire_name.eq_ignore_ascii_case("accept")
            && !operation.responses.accept.is_empty())
        .then(|| operation.responses.accept.join(", "));
        if let Some(configured) = fallback {
            pieces.push(quote! {
                if let ::std::option::Option::Some(value) = &self.params.#field {
                    #body
                } else {
                    request = request.header(::reqwest::header::ACCEPT, #configured);
                }
            });
        } else {
            pieces.push(read_param(param, &field, &body));
        }
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
            read_param(param, &field, &body)
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

/// The `Accept` header a configured response media type earns.
///
/// Emitted only where [`Config::media_types`](crate::config::Config::media_types) chose at one
/// of this operation's response positions — the choice is an argument with the server, and a
/// request that never states it leaves the server free to answer with the type the caller
/// refused. A declared `Accept` header parameter outranks it: the document already gave the
/// caller that knob, and two spellings of one header is a worse wire than either alone.
fn accept_header(operation: &OperationContract) -> TokenStream {
    if operation.responses.accept.is_empty()
        || operation.params.iter().any(|param| {
            param.style.location() == Location::Header
                && param.wire_name.eq_ignore_ascii_case("accept")
        })
    {
        return TokenStream::new();
    }
    let value = operation.responses.accept.join(", ");
    quote! {
        request = request.header(::reqwest::header::ACCEPT, #value);
    }
}

/// Read one input and do `body` with `value` bound to a reference to it.
///
/// A required input is a plain params field and reads directly; an optional one is an `if let`
/// where an unset value simply contributes nothing. Nothing here can refuse: whatever the
/// description requires already existed when the params struct was built.
fn read_param(
    param: &ParamContract,
    field: &proc_macro2::Ident,
    body: &TokenStream,
) -> TokenStream {
    if param.required {
        quote! {
            {
                let value = &self.params.#field;
                #body
            }
        }
    } else {
        quote! {
            if let ::std::option::Option::Some(value) = &self.params.#field {
                #body
            }
        }
    }
}

/// The header parameter the description names `Content-Type`, when it declares one.
///
/// Real documents do (cloudflare's R2 object upload stores the caller's type), and it makes the
/// request's content type *caller data*: the automatic body header has to yield to it, or the
/// wire carries two disagreeing values and the server reads whichever arrived first.
pub(super) fn content_type_param(operation: &OperationContract) -> Option<&ParamContract> {
    operation
        .params_at(Location::Header)
        .find(|param| param.wire_name.eq_ignore_ascii_case("content-type"))
}

/// Whether `send()` needs the `declared_content_type` flag: an *optional* declared
/// `Content-Type` parameter beside a body whose arm writes an explicit content type. A required
/// parameter always writes, so the body simply never does; plain `application/json` needs
/// nothing, because reqwest's `json` only fills the header in when it is absent; a multipart
/// body's type carries the boundary and is never overridden.
fn needs_content_type_flag(operation: &OperationContract) -> bool {
    let Some(param) = content_type_param(operation) else {
        return false;
    };
    !param.required
        && match &operation.body {
            Some(BodyContract::Json { content_type, .. }) => content_type != "application/json",
            Some(
                BodyContract::Form { .. } | BodyContract::Text { .. } | BodyContract::Bytes { .. },
            ) => true,
            Some(BodyContract::Multipart { .. }) | None => false,
        }
}

/// The body's own content-type write, deferring to a declared `Content-Type` parameter.
fn body_content_type(operation: &OperationContract, value: &str) -> TokenStream {
    match content_type_param(operation) {
        // Required: the parameter always wrote the header, and appending a second value would
        // put two disagreeing spellings on the wire.
        Some(param) if param.required => TokenStream::new(),
        Some(_) => quote! {
            if !declared_content_type {
                request = request.header(::reqwest::header::CONTENT_TYPE, #value);
            }
        },
        None => quote! {
            request = request.header(::reqwest::header::CONTENT_TYPE, #value);
        },
    }
}

fn request_body(operation: &OperationContract, operation_name: &str) -> TokenStream {
    let Some(contract) = &operation.body else {
        return TokenStream::new();
    };
    let inner = match contract {
        BodyContract::Json { content_type, .. } => {
            if content_type == "application/json" {
                quote! { request = request.json(body); }
            } else {
                // reqwest's `json` writes `application/json` only when the header is absent, so
                // the declared spelling set first is the one the wire carries.
                let set = body_content_type(operation, content_type);
                quote! {
                    #set
                    request = request.json(body);
                }
            }
        }
        BodyContract::Form { specs, .. } => {
            let specs = form_specs(specs);
            let set = body_content_type(operation, "application/x-www-form-urlencoded");
            quote! {
                let value = support::to_value(body, #operation_name).map_err(Error::Decode)?;
                #set
                request = request.body(support::style::form_body(&value, #specs));
            }
        }
        BodyContract::Multipart { parts, .. } => {
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
        // Text and bytes write the same statement; what differs is the *type* the params struct
        // holds, which `body_type` decides. `reqwest::Body` takes both a `String` and a `Vec<u8>`.
        BodyContract::Text { content_type, .. } | BodyContract::Bytes { content_type, .. } => {
            let set = body_content_type(operation, content_type);
            quote! {
                #set
                request = request.body(body.clone());
            }
        }
    };
    if contract.required() {
        quote! {
            {
                let body = &self.params.body;
                #inner
            }
        }
    } else {
        quote! {
            if let ::std::option::Option::Some(body) = &self.params.body {
                #inner
            }
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
    config: &Config,
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

    let success_arms = arm_matches(&successes, success, successes.len() > 1, false, config);
    let failure_arms = arm_matches(&failures, failure, wrapped_failure, true, config);
    let ranges: BTreeSet<u8> = operation
        .responses
        .arms
        .iter()
        .filter_map(|arm| match arm.status {
            crate::api::StatusPattern::Range(hundreds) => Some(hundreds),
            crate::api::StatusPattern::Exact(_) => None,
        })
        .collect();
    // Clippy reports this lint only when the exact arm is the range's lower bound; an interior
    // exact arm such as `404` before `4XX` is intentionally accepted, so expecting it there would
    // create an unfulfilled-lint warning in the generated crate.
    let overlapping = operation.responses.arms.iter().any(|arm| match arm.status {
        crate::api::StatusPattern::Exact(code) => {
            code % 100 == 0 && ranges.contains(&u8::try_from(code / 100).unwrap_or(u8::MAX))
        }
        crate::api::StatusPattern::Range(_) => false,
    });
    let overlap = overlapping.then(|| {
        quote! {
            #[expect(
                clippy::match_overlapping_arm,
                reason = "OpenAPI gives an exact status precedence over its declared status range"
            )]
        }
    });
    let fallback = match (&operation.responses.default, claims_success) {
        // No `default`: a status the document never mentioned has no shape progeny has ever seen.
        (None, _) => quote! { _ => ::std::result::Result::Err(Error::UnexpectedStatus(response)), },
        // A `default` beside declared successes catches what they do not, which is a failure.
        (Some(arm), false) => {
            let decoded = decode(arm, failure, wrapped_failure, config);
            quote! { _ => ::std::result::Result::Err(Error::Declared(#decoded)), }
        }
        // A `default` and nothing else: it is the whole contract, so a 2xx through it succeeded.
        (Some(arm), true) => {
            let ok = decode(arm, success, false, config);
            let err = decode(arm, failure, wrapped_failure, config);
            quote! {
                200..=299 => ::std::result::Result::Ok(#ok),
                _ => ::std::result::Result::Err(Error::Declared(#err)),
            }
        }
    };
    quote! {
        let status = response.status().as_u16();
        // OpenAPI says an exact status claims a response before a range does, so a document
        // declaring both `400` and `4XX` gets deliberately overlapping arms in that order.
        #overlap
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
    config: &Config,
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
        let decoded = decode(arm, ty, wrapped, config);
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
fn decode(arm: &ResponseArm, ty: &TokenStream, wrapped: bool, config: &Config) -> TokenStream {
    let decoded = match &arm.body {
        ResponseBody::Json { .. } | ResponseBody::Empty => {
            quote! { support::decode_json(response).await? }
        }
        ResponseBody::Text { .. } => quote! { support::decode_text(response).await? },
        ResponseBody::Bytes { .. } => match config.formats.bytes {
            BytesRepr::Vec => quote! { support::decode_bytes(response).await? },
            BytesRepr::Bytes => {
                quote! {
                    support::decode_bytes(response).await?
                        .map(::bytes::Bytes::from)
                }
            }
        },
    };
    if !wrapped {
        return decoded;
    }
    let variant = format_ident!("{}", arm.rust_name.as_str());
    if response_body_is_boxed(&arm.body) {
        quote! {
            #decoded
                .map(::std::boxed::Box::new)
                .map(#ty::#variant)
        }
    } else {
        quote! { #decoded.map(#ty::#variant) }
    }
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

/// Whether the operation takes any input beyond the client itself.
///
/// One with none gets no params struct at all: an accessor with no argument states "no input"
/// better than an empty literal would, and gaining a first input still changes the accessor's
/// signature, so the compile error on drift is kept.
pub(crate) fn takes_params(operation: &OperationContract) -> bool {
    !operation.params.is_empty() || operation.body.is_some()
}

/// `FooParams`: the caller-built struct of every input the operation takes.
pub(crate) fn params_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!("{}Params", camel(operation))
}

/// `FooRequest`: the params bound to a client, ready to send.
fn request_name(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!("{}Request", camel(operation))
}

fn camel(operation: &OperationContract) -> String {
    operation
        .rust_name
        .as_str()
        .split('_')
        .map(capitalize)
        .collect()
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
