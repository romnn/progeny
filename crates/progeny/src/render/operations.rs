//! Reflection: the operations a description declares, as data.
//!
//! The third renderer over the finalized [`ApiModel`], beside [`super::client`] and
//! [`super::server`]. A program built around the generated code needs to *name* an operation in
//! Rust, ask it a few static facts, and map what a running router matched back to that name — a
//! mock keying scripted faults by route, middleware labelling traces by operation, a conformance
//! runner iterating every operation. Without this module each of them restates method and path
//! strings by hand and drifts the moment the description changes.
//!
//! Two invariants are carried by construction rather than by a check:
//!
//! * **The three renderers cannot disagree.** `Route::path()` is
//!   [`RegistrableRoute::path()`](crate::api::RegistrableRoute::path), the string the router
//!   registers — and the router registers *through* `Route::X.path()`, so removing or renaming a
//!   route here fails the server's compile rather than a request.
//! * **A `Route` exists iff a handler exists.** Both read `operation.registrable.is_some()`.
//!
//! The per-operation facts live in one table indexed by discriminant, so the module costs a row
//! per operation rather than a match arm per accessor, and every template literal occurs once. It
//! has no dependency and is emitted whenever the model has operations, in every packaging: a flag
//! would be a knob nothing needs plus a validity rule for `generate` to carry.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::api::{ApiModel, Method, OperationContract};
use crate::contract::RustIdent;

/// Render the `operations` module.
pub(super) fn render(model: &ApiModel) -> TokenStream {
    // One variant per operation object of the description. A body variant is the primary's
    // operation under another media type — it shares the route and is never registrable — so it
    // gets no variant, and `(method, path)` identifies at most one `Route`.
    let operations: Vec<&OperationContract> = model
        .operations()
        .iter()
        .filter(|operation| !operation.body_variant)
        .collect();
    let routes: Vec<&OperationContract> = operations
        .iter()
        .copied()
        .filter(|operation| operation.registrable.is_some())
        .collect();
    let operation_variants: Vec<proc_macro2::Ident> = operations
        .iter()
        .map(|operation| variant(operation))
        .collect();
    let route_variants: Vec<proc_macro2::Ident> =
        routes.iter().map(|operation| variant(operation)).collect();

    let enums = enums(&operation_variants, &route_variants);
    let methods = method_impl();
    let tables = tables(&operations, &route_variants);
    let accessors = accessors(&operation_variants, &route_variants);
    // `allow` rather than `expect`, and deliberately: whether `enum_variant_names` fires depends
    // on the consumer's clippy configuration (`enum-variant-name-threshold`) and on the
    // description's own naming habits — a read-only API whose every operation starts with
    // `get`, cloudflare's `…CreateRoute` inside `Route` — so an expectation would be unfulfilled
    // exactly as often as it was needed, and an unfulfilled expectation is itself a warning in
    // the consumer's build. Inner rather than outer, because `clippy::allow_attributes` lints
    // only the outer form, and a consumer denying it must still be able to build this module.
    quote! {
        #![doc = " The operations this description declares, as data."]
        #![doc = ""]
        #![doc = " Rendered from the same finalized model as `client` and `server`, so the three cannot"]
        #![doc = " disagree. What is here is what progeny generated: an operation progeny dropped is absent,"]
        #![doc = " and one the router refused is an `Operation` without a `Route`."]
        #![allow(
            clippy::enum_variant_names,
            clippy::upper_case_acronyms,
            reason = "the variants are the description's operation names as progeny spells them everywhere else, whatever pattern they happen to share"
        )]

        #enums
        #methods
        #tables
        #accessors
    }
}

/// The variant that names an operation in both `Operation` and `Route`: the type stem every
/// other generated item for the operation is built from, escaped where a stem is a keyword
/// (`self` → `Self_`).
pub(crate) fn variant(operation: &OperationContract) -> proc_macro2::Ident {
    format_ident!(
        "{}",
        RustIdent::stem_variant(&operation.rust_name.type_stem()).as_str()
    )
}

/// The three enums: every operation, the registrable subset, and the closed set of methods.
fn enums(
    operation_variants: &[proc_macro2::Ident],
    route_variants: &[proc_macro2::Ident],
) -> TokenStream {
    let methods = METHODS
        .iter()
        .map(|(variant, _)| format_ident!("{variant}"));
    quote! {
        /// One operation of the description. Exhaustive on purpose: a per-operation table stops
        /// compiling when the description gains, loses, or renames an operation.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Operation {
            #(#operation_variants,)*
        }

        /// One route `server::router()` registers: the operations a real router accepted.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Route {
            #(#route_variants,)*
        }

        /// The HTTP method an operation is declared under: the eight path-item methods progeny
        /// supports. A method a later specification adds is a new variant, and therefore a
        /// deliberate break.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Method {
            #(#methods,)*
        }
    }
}

/// `Method`'s two conversions, one arm per method each way.
fn method_impl() -> TokenStream {
    let as_str = METHODS.iter().map(|(variant, token)| {
        let variant = format_ident!("{variant}");
        quote! { Self::#variant => #token, }
    });
    let from_token = METHODS.iter().map(|(variant, token)| {
        let variant = format_ident!("{variant}");
        quote! { #token => ::std::option::Option::Some(Self::#variant), }
    });
    quote! {
        impl Method {
            /// The token on the request line, `GET`.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    #(#as_str)*
                }
            }

            /// The method for a request-line token; `None` for any other method.
            #[must_use]
            pub fn from_token(token: &str) -> ::std::option::Option<Self> {
                match token {
                    #(#from_token)*
                    _ => ::std::option::Option::None,
                }
            }
        }
    }
}

/// The per-operation table, and the route-to-operation table when there is a route to index.
///
/// `static` rather than `const`, because clippy's `large_const_arrays` fires on a table past
/// 16 KiB — cloudflare's is 3,200 rows — and a const array is copied wherever it is read, while
/// a `const fn` may read a `static` since Rust 1.83.
fn tables(operations: &[&OperationContract], route_variants: &[proc_macro2::Ident]) -> TokenStream {
    let entries = operations.iter().map(|operation| {
        let rust_name = operation.rust_name.as_str();
        let method = method_variant(operation.method);
        // The registered template where there is one, so that `Route::path()` is by construction
        // the string `server::router()` registers; the template as written otherwise, which is
        // the same spelling and only says where a client would send the request.
        let path = operation.registrable.as_ref().map_or_else(
            || operation.path.to_string(),
            |route| route.path().to_owned(),
        );
        let route = operation.registrable.as_ref().map_or_else(
            || quote! { ::std::option::Option::None },
            |_| {
                let variant = variant(operation);
                quote! { ::std::option::Option::Some(Route::#variant) }
            },
        );
        quote! {
            Entry { rust_name: #rust_name, method: Method::#method, path: #path, route: #route },
        }
    });
    let operation_count = proc_macro2::Literal::usize_unsuffixed(operations.len());
    let route_count = proc_macro2::Literal::usize_unsuffixed(route_variants.len());
    // An empty `Route` is a real shape — a description whose every route the router refused has
    // a client and no server — and the table it would index is not emitted at all.
    let routes = (!route_variants.is_empty()).then(|| {
        quote! {
            /// The operation behind every route, indexed by the route's discriminant.
            static ROUTES: [Operation; #route_count] = [#(Operation::#route_variants),*];
        }
    });
    quote! {
        /// One row of the per-operation table.
        struct Entry {
            rust_name: &'static str,
            method: Method,
            path: &'static str,
            route: ::std::option::Option<Route>,
        }

        /// The facts of every operation, indexed by its discriminant.
        static TABLE: [Entry; #operation_count] = [#(#entries)*];

        #routes
    }
}

/// The accessors: one line each over the tables, and `from_matched`.
fn accessors(
    operation_variants: &[proc_macro2::Ident],
    route_variants: &[proc_macro2::Ident],
) -> TokenStream {
    // An empty enum cannot be cast to an index, so its one accessor is the empty match.
    let route_operation = if route_variants.is_empty() {
        quote! { match self {} }
    } else {
        quote! { ROUTES[self as usize] }
    };
    quote! {
        impl Operation {
            /// Every operation, in the model's order: by path template, then by method in the
            /// order a path item lists them. Stable across runs of the same description.
            pub const ALL: &[Self] = &[#(Self::#operation_variants),*];

            /// The Rust name progeny gave the operation: the client method, the server trait
            /// method, the `pagination` configuration key, and the label a server `Rejection`
            /// carries. Derived from `operationId` with collision suffixes, so not stable across
            /// revisions.
            #[must_use]
            pub const fn rust_name(self) -> &'static str {
                TABLE[self as usize].rust_name
            }

            /// The method the description declares the operation under.
            #[must_use]
            pub const fn method(self) -> Method {
                TABLE[self as usize].method
            }

            /// The path template as the description writes it, with `{name}` variables.
            #[must_use]
            pub const fn path(self) -> &'static str {
                TABLE[self as usize].path
            }

            /// The route the server registers for this operation, when the router accepted it.
            #[must_use]
            pub const fn route(self) -> ::std::option::Option<Route> {
                TABLE[self as usize].route
            }
        }

        impl Route {
            /// Every registered route, in the order of [`Operation::ALL`].
            pub const ALL: &[Self] = &[#(Self::#route_variants),*];

            /// The operation this route serves.
            #[must_use]
            pub const fn operation(self) -> Operation {
                #route_operation
            }

            /// The declared method. axum also dispatches `HEAD` to a `GET` handler.
            #[must_use]
            pub const fn method(self) -> Method {
                self.operation().method()
            }

            /// The template `server::router()` registers, which is what axum's `MatchedPath`
            /// reports when the router is mounted at the root; strip any nesting prefix before
            /// matching.
            #[must_use]
            pub const fn path(self) -> &'static str {
                self.operation().path()
            }

            /// The route that served a request, from its method and matched template: exact
            /// first, then `HEAD` falls back to the `GET` route axum dispatched it to. Linear over
            /// `ALL`; a hot middleware indexes `ALL` once.
            #[must_use]
            pub fn from_matched(method: Method, path: &str) -> ::std::option::Option<Self> {
                let declared = |method: Method| {
                    Self::ALL
                        .iter()
                        .copied()
                        .find(|route| route.method() == method && route.path() == path)
                };
                declared(method).or_else(|| {
                    if method == Method::Head {
                        declared(Method::Get)
                    } else {
                        ::std::option::Option::None
                    }
                })
            }
        }
    }
}

/// The generated `Method` variant and request-line token of each method, in declaration order.
pub(super) const METHODS: [(&str, &str); 8] = [
    ("Get", "GET"),
    ("Put", "PUT"),
    ("Post", "POST"),
    ("Delete", "DELETE"),
    ("Options", "OPTIONS"),
    ("Head", "HEAD"),
    ("Patch", "PATCH"),
    ("Trace", "TRACE"),
];

/// The generated `Method` variant for a declared method.
///
/// Spelled arm by arm so that a method the document model gains is a compile error here rather
/// than a silently missing variant: the generated enum is the closed set progeny supports.
fn method_variant(method: Method) -> proc_macro2::Ident {
    let name = match method {
        Method::Get => "Get",
        Method::Put => "Put",
        Method::Post => "Post",
        Method::Delete => "Delete",
        Method::Options => "Options",
        Method::Head => "Head",
        Method::Patch => "Patch",
        Method::Trace => "Trace",
    };
    format_ident!("{name}")
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::json;

    use super::{render, variant};
    use crate::api::tests::{model_of, with_paths};

    /// The tokens as one whitespace-free string, for comparing spellings rather than layouts.
    fn spelled(tokens: &impl ToTokensExt) -> String {
        tokens.spell()
    }

    trait ToTokensExt {
        fn spell(&self) -> String;
    }

    impl<T: quote::ToTokens> ToTokensExt for T {
        fn spell(&self) -> String {
            self.to_token_stream()
                .to_string()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        }
    }

    fn enum_variants(file: &syn::File, name: &str) -> eyre::Result<Vec<String>> {
        file.items
            .iter()
            .find_map(|item| match item {
                syn::Item::Enum(item) if item.ident == name => Some(
                    item.variants
                        .iter()
                        .map(|variant| variant.ident.to_string())
                        .collect(),
                ),
                _ => None,
            })
            .ok_or_eyre(format!("the module declares `enum {name}`"))
    }

    fn static_elements(file: &syn::File, name: &str) -> Option<Vec<syn::Expr>> {
        file.items.iter().find_map(|item| match item {
            syn::Item::Static(item) if item.ident == name => match &*item.expr {
                syn::Expr::Array(array) => Some(array.elems.iter().cloned().collect()),
                _ => None,
            },
            _ => None,
        })
    }

    /// The elements of `impl <owner> { pub const ALL: &[Self] = &[…]; }`.
    fn all_of(file: &syn::File, owner: &str) -> eyre::Result<Vec<String>> {
        let elements = file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Impl(item) if item.self_ty.spell() == owner => Some(&item.items),
                _ => None,
            })
            .flatten()
            .find_map(|item| match item {
                syn::ImplItem::Const(constant) if constant.ident == "ALL" => match &constant.expr {
                    syn::Expr::Reference(reference) => match &*reference.expr {
                        syn::Expr::Array(array) => {
                            Some(array.elems.iter().map(spelled).collect::<Vec<_>>())
                        }
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            })
            .ok_or_eyre(format!("`{owner}::ALL` is a reference to an array"))?;
        Ok(elements)
    }

    /// One `Entry { … }` literal's fields, by name.
    fn entry_fields(entry: &syn::Expr) -> eyre::Result<std::collections::BTreeMap<String, String>> {
        let syn::Expr::Struct(literal) = entry else {
            eyre::bail!("a table row is a struct literal, not `{}`", spelled(entry));
        };
        Ok(literal
            .fields
            .iter()
            .map(|field| (field.member.spell(), spelled(&field.expr)))
            .collect())
    }

    /// Three methods on one template, two templates that differ only in their variable's name —
    /// the router registers the first in path order and refuses the second — and a request body
    /// declaring two media types, whose second client method is the same operation and gets no
    /// variant.
    fn mixed_model() -> eyre::Result<crate::api::ApiModel> {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}},
                "post": {"operationId": "createPets", "responses": {"201": {"description": "made"}}},
                "head": {"operationId": "headPets", "responses": {"200": {"description": "ok"}}},
            },
            "/pets/{petId}": {
                "get": {
                    "operationId": "showPetById",
                    "parameters": [{"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
            "/pets/{id}": {
                "delete": {
                    "operationId": "deletePet",
                    "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"204": {"description": "gone"}},
                },
            },
            "/upload": {
                "post": {
                    "operationId": "upload",
                    "requestBody": {"required": true, "content": {
                        "application/json": {"schema": {"type": "object"}},
                        "multipart/form-data": {"schema": {"type": "object"}},
                    }},
                    "responses": {"204": {"description": "stored"}},
                },
            },
        })))?;
        Ok(model)
    }

    #[test_util::test]
    fn the_enums_mirror_the_model_in_its_order() {
        let model = mixed_model()?;
        let file = syn::parse2::<syn::File>(render(&model))?;

        // Every operation object once, in model order; the body variant is absent.
        let operations: Vec<&crate::api::OperationContract> = model
            .operations()
            .iter()
            .filter(|operation| !operation.body_variant)
            .collect();
        let expected: Vec<String> = operations
            .iter()
            .map(|operation| variant(operation).to_string())
            .collect();
        assert_eq!(enum_variants(&file, "Operation")?, expected);
        assert_eq!(
            expected,
            [
                "ListPets",
                "CreatePets",
                "HeadPets",
                "DeletePet",
                "ShowPetById",
                "Upload"
            ]
        );
        assert!(
            model
                .operations()
                .iter()
                .any(|operation| operation.body_variant),
            "the fixture declares a body variant"
        );
        assert!(
            !expected.iter().any(|name| name.contains("Multipart")),
            "{expected:?}"
        );
        assert_eq!(
            all_of(&file, "Operation")?,
            expected
                .iter()
                .map(|name| format!("Self::{name}"))
                .collect::<Vec<_>>()
        );

        // The registrable subset, in the same order.
        let routes: Vec<String> = operations
            .iter()
            .filter(|operation| operation.registrable.is_some())
            .map(|operation| variant(operation).to_string())
            .collect();
        assert_eq!(enum_variants(&file, "Route")?, routes);
        // `/pets/{id}` sorts before `/pets/{petId}` and wins the template; the loser is an
        // `Operation` without a `Route`.
        assert_eq!(
            routes,
            ["ListPets", "CreatePets", "HeadPets", "DeletePet", "Upload"]
        );
        assert_eq!(
            all_of(&file, "Route")?,
            routes
                .iter()
                .map(|name| format!("Self::{name}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            static_elements(&file, "ROUTES")
                .ok_or_eyre("the routes table")?
                .iter()
                .map(spelled)
                .collect::<Vec<_>>(),
            routes
                .iter()
                .map(|name| format!("Operation::{name}"))
                .collect::<Vec<_>>()
        );
    }

    /// Row `i` of the table is operation `i`: its name, its method, the template the router
    /// registers, and its route exactly when the classifier accepted one.
    #[test_util::test]
    fn the_table_row_of_an_operation_is_its_discriminant() {
        let model = mixed_model()?;
        let file = syn::parse2::<syn::File>(render(&model))?;
        let operations: Vec<&crate::api::OperationContract> = model
            .operations()
            .iter()
            .filter(|operation| !operation.body_variant)
            .collect();
        let table = static_elements(&file, "TABLE").ok_or_eyre("the operation table")?;
        assert_eq!(table.len(), operations.len());
        for (operation, row) in operations.iter().zip(&table) {
            let fields = entry_fields(row)?;
            assert_eq!(
                fields.get("rust_name").map(String::as_str),
                Some(format!("{:?}", operation.rust_name.as_str()).as_str())
            );
            assert_eq!(
                fields.get("method").map(String::as_str),
                Some(format!("Method::{}", super::method_variant(operation.method)).as_str())
            );
            let path = operation.registrable.as_ref().map_or_else(
                || operation.path.to_string(),
                |route| route.path().to_owned(),
            );
            assert_eq!(
                fields.get("path").map(String::as_str),
                Some(format!("{path:?}").as_str())
            );
            let route = match operation.registrable {
                Some(_) => format!("::std::option::Option::Some(Route::{})", variant(operation)),
                None => "::std::option::Option::None".to_owned(),
            };
            assert_eq!(fields.get("route"), Some(&route), "{}", operation.rust_name);
        }
    }

    #[test_util::test]
    fn a_description_whose_every_route_was_refused_has_operations_and_no_routes() {
        // `:id` is the axum 0.7 capture spelling, which the router refuses at startup and the
        // classifier therefore refuses at generation time: a client, and no server.
        let (model, _) = model_of(with_paths(json!({
            "/pets/:id": {
                "get": {"operationId": "showPet", "responses": {"200": {"description": "ok"}}},
            },
            "/owners/:id": {
                "get": {"operationId": "showOwner", "responses": {"200": {"description": "ok"}}},
            },
        })))?;
        assert!(
            model
                .operations()
                .iter()
                .all(|operation| operation.registrable.is_none()),
            "the fixture's routes are all refused"
        );
        let file = syn::parse2::<syn::File>(render(&model))?;
        assert_eq!(enum_variants(&file, "Operation")?, ["ShowOwner", "ShowPet"]);
        assert!(enum_variants(&file, "Route")?.is_empty());
        assert!(all_of(&file, "Route")?.is_empty());
        // An empty enum cannot be cast to an index, so its accessor is the empty match and the
        // table it would index is not emitted at all.
        assert!(static_elements(&file, "ROUTES").is_none());
        let rendered = render(&model).to_string();
        assert!(rendered.contains("match self { }"), "{rendered}");
    }

    #[test_util::test]
    fn a_variant_is_the_type_stem_escaped_only_where_a_stem_is_a_keyword() {
        let (model, _) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "get_x_y", "responses": {"204": {"description": "ok"}}}},
            "/b": {"get": {"operationId": "2fa_verify", "responses": {"204": {"description": "ok"}}}},
            "/c": {"get": {"operationId": "self", "responses": {"204": {"description": "ok"}}}},
            "/d": {"get": {"operationId": "String", "responses": {"204": {"description": "ok"}}}},
        })))?;
        let file = syn::parse2::<syn::File>(render(&model))?;
        assert_eq!(
            enum_variants(&file, "Operation")?,
            ["GetXY", "_2faVerify", "Self_", "String"]
        );
        // The names the rows carry are the method names, keyword suffix and all.
        let table = static_elements(&file, "TABLE").ok_or_eyre("the operation table")?;
        let names: Vec<String> = table
            .iter()
            .map(|row| {
                entry_fields(row).map(|fields| fields.get("rust_name").cloned().unwrap_or_default())
            })
            .collect::<eyre::Result<_>>()?;
        assert_eq!(
            names,
            ["\"get_x_y\"", "\"_2fa_verify\"", "\"self_\"", "\"string\""]
        );
    }

    #[test_util::test]
    fn the_method_enum_is_the_closed_set_the_document_model_walks() {
        let (model, _) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "a", "responses": {"204": {"description": "ok"}}}},
        })))?;
        let file = syn::parse2::<syn::File>(render(&model))?;
        let variants = enum_variants(&file, "Method")?;
        assert_eq!(
            variants,
            super::METHODS
                .iter()
                .map(|(variant, _)| (*variant).to_owned())
                .collect::<Vec<_>>()
        );
        // Every token both ways, and one that is not a method.
        let rendered = render(&model).to_string();
        for (variant, token) in super::METHODS {
            assert!(
                rendered.contains(&format!("Self :: {variant} => {token:?}")),
                "{rendered}"
            );
            assert!(
                rendered.contains(&format!(
                    "{token:?} => :: std :: option :: Option :: Some (Self :: {variant})"
                )),
                "{rendered}"
            );
        }
    }
}
