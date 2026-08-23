//! Rendering: pure functions from frozen contracts to source text.
//!
//! Three packagings of one rendering. **Crate mode** writes one manifest and `src` tree;
//! **Workspace mode** places the types, client, and server behind crate boundaries; **module mode**
//! writes the same modules nested into one file, for `include!` from a build script or for checking
//! in. One renderer, three wrappers, no forked logic — so a change to how a type is written cannot
//! land in one packaging and not the others.
//!
//! **Determinism is an output invariant**: the same document, configuration and progeny version
//! render byte-identical source. Everything here iterates `BTreeMap`s and `Vec`s in a fixed order,
//! and the corpus harness generates every document twice per run and compares.

// `client`, `server` and `types` are crate-visible rather than private because the probe harness
// reuses their naming — type paths, group and response names — instead of growing a second copy
// that could drift from what is actually rendered.
pub(crate) mod client;
mod manifest;
mod serde_impl;
pub(crate) mod server;
pub(crate) mod types;

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use proc_macro2::TokenStream;
use quote::quote;

use crate::api::ApiModel;
use crate::config::{Config, Packaging};
use crate::contract::Contracts;

/// Render the contracts and the API model into the files a caller writes out.
pub(crate) fn run(
    contracts: &Contracts,
    api: &ApiModel,
    config: &Config,
) -> BTreeMap<Utf8PathBuf, String> {
    // `emit.types = false` cannot arrive here: `generate` refuses it, because an empty rendering
    // with a success code would be a silent no-op.
    let mut files = BTreeMap::new();
    // An operation nobody can call is not worth a module: a description with no paths generates a
    // type layer and stops there, rather than emitting a `Client` with no methods and the whole
    // HTTP dependency behind it.
    let has_operations = !api.operations().is_empty();
    let http = config.emit.client && has_operations;
    // Whether any operation declared pagination, which is what decides whether the generated crate
    // depends on the stream machinery at all.
    let streams = http
        && api
            .operations()
            .iter()
            .any(|operation| operation.pagination.is_some());
    // The same rule on the serving side, with one extra condition: a description whose every route
    // the router refused has no trait methods to implement, and a trait nobody can implement is not
    // a module.
    let serves = config.emit.server
        && api
            .operations()
            .iter()
            .any(|operation| operation.registrable.is_some());
    let external_api_modules = config.packaging == Packaging::Workspace;
    let mut types_body =
        types::render(contracts, api, http || serves, external_api_modules, config);
    types_body.extend(serde_impl::render(contracts));
    let client_body = http.then(|| client::render(api, contracts, config));
    let server_body = serves.then(|| server::render(api, contracts, config));
    let mut modules = vec![("types", types_body.clone())];

    if let Some(body) = &client_body {
        modules.push(("client", body.clone()));
    }
    if let Some(body) = &server_body {
        modules.push(("server", body.clone()));
    }
    // Emitted only when something calls into it: an unused module is compile time the consumer did
    // not ask for. An upload field counts — the types module spells `support::Upload` even when
    // no hand-written serde and no edge asked for the module.
    if http || serves || serde_impl::needed(contracts) || contracts.uses_upload() {
        modules.push((
            "support",
            crate::support::tokens(
                http,
                serves,
                config.packaging == Packaging::Crate,
                config.body_limit,
            ),
        ));
    }

    match config.packaging {
        Packaging::Crate => {
            files.insert(
                Utf8PathBuf::from("Cargo.toml"),
                manifest::render(contracts, api, config, http, serves, streams),
            );
            let declarations = modules.iter().map(|(name, _)| {
                let ident = quote::format_ident!("{name}");
                // The support module is not part of the generated crate's public API; it is
                // reachable because the type modules call into it and for no other reason.
                let hidden = (*name == "support").then(|| quote! { #[doc(hidden)] });
                let gated = match *name {
                    "client" => quote! { #[cfg(feature = "client")] },
                    "server" => quote! { #[cfg(feature = "server")] },
                    _ => quote! {},
                };
                quote! { #hidden #gated pub mod #ident; }
            });
            files.insert(
                Utf8PathBuf::from("src/lib.rs"),
                source(&quote! {
                    #![doc = " Generated by progeny. Edits are overwritten on the next run."]
                    #(#declarations)*
                }),
            );
            for (name, tokens) in modules {
                files.insert(Utf8PathBuf::from(format!("src/{name}.rs")), source(&tokens));
            }
        }
        Packaging::Module => {
            let nested = modules.into_iter().map(|(name, tokens)| {
                let ident = quote::format_ident!("{name}");
                let hidden = (name == "support").then(|| quote! { #[doc(hidden)] });
                quote! { #hidden pub mod #ident { #tokens } }
            });
            // A line comment rather than the inner `#![doc]` the other two packagings carry: this
            // file exists to be `include!`d, and an inner attribute is only legal at the start of
            // the *including* file — so the notice that says the file is generated would be the
            // one thing stopping it from being used the way it is meant to be.
            files.insert(
                Utf8PathBuf::from("progeny.rs"),
                format!(
                    "// Generated by progeny. Edits are overwritten on the next run.\n{}",
                    source(&quote! { #(#nested)* })
                ),
            );
        }
        Packaging::Workspace => {
            files.extend(workspace_files(
                contracts,
                api,
                config,
                streams,
                &types_body,
                client_body.as_ref(),
                server_body.as_ref(),
            ));
        }
    }
    files
}

fn workspace_files(
    contracts: &Contracts,
    api: &ApiModel,
    config: &Config,
    streams: bool,
    types_body: &TokenStream,
    client_body: Option<&TokenStream>,
    server_body: Option<&TokenStream>,
) -> BTreeMap<Utf8PathBuf, String> {
    let mut files = BTreeMap::new();
    files.insert(
        Utf8PathBuf::from("Cargo.toml"),
        manifest::workspace(config, client_body.is_some(), server_body.is_some()),
    );
    files.insert(
        Utf8PathBuf::from("README.md"),
        manifest::workspace_readme(config, client_body.is_some(), server_body.is_some()),
    );
    insert_types_crate(&mut files, contracts, config, types_body);
    let types_name = format!("{}-types", config.package.name);
    let types_crate = quote::format_ident!("{}", types_name.replace('-', "_"));
    if let Some(body) = client_body {
        insert_client_crate(&mut files, api, config, streams, &types_crate, body);
    }
    if let Some(body) = server_body {
        insert_server_crate(&mut files, api, config, &types_crate, body);
    }
    files
}

fn insert_types_crate(
    files: &mut BTreeMap<Utf8PathBuf, String>,
    contracts: &Contracts,
    config: &Config,
    body: &TokenStream,
) {
    let directory = Utf8PathBuf::from(format!("{}-types", config.package.name));
    files.insert(
        directory.join("Cargo.toml"),
        manifest::types_crate(contracts, config),
    );
    // `pub` for the same reason the one-crate packaging makes it public: an upload body field is
    // `support::Upload`, and a consumer who has to fill that field has to be able to name the
    // type — a private module would make the field itself a `private_interfaces` defect.
    let needed = serde_impl::needed(contracts) || contracts.uses_upload();
    let support = needed.then(|| {
        quote! {
            #[doc(hidden)]
            pub mod support;
        }
    });
    files.insert(
        directory.join("src/lib.rs"),
        source(&quote! {
            #![doc = " Generated by progeny. Edits are overwritten on the next run."]
            #support
            pub mod types;
        }),
    );
    files.insert(directory.join("src/types.rs"), source(body));
    if needed {
        files.insert(
            directory.join("src/support.rs"),
            source(&crate::support::types_tokens()),
        );
    }
}

fn insert_client_crate(
    files: &mut BTreeMap<Utf8PathBuf, String>,
    api: &ApiModel,
    config: &Config,
    streams: bool,
    types_crate: &syn::Ident,
    body: &TokenStream,
) {
    let directory = Utf8PathBuf::from(format!("{}-client", config.package.name));
    files.insert(
        directory.join("Cargo.toml"),
        manifest::client_crate(api, config, streams),
    );
    files.insert(
        directory.join("src/lib.rs"),
        source(&quote! {
            #![doc = " Generated by progeny. Edits are overwritten on the next run."]
            pub use #types_crate::types;
            #[doc(hidden)]
            mod support;
            pub mod client;
        }),
    );
    files.insert(directory.join("src/client.rs"), source(body));
    files.insert(
        directory.join("src/support.rs"),
        source(&crate::support::client_tokens(client_use(api))),
    );
}

fn insert_server_crate(
    files: &mut BTreeMap<Utf8PathBuf, String>,
    api: &ApiModel,
    config: &Config,
    types_crate: &syn::Ident,
    body: &TokenStream,
) {
    let directory = Utf8PathBuf::from(format!("{}-server", config.package.name));
    files.insert(
        directory.join("Cargo.toml"),
        manifest::server_crate(api, config),
    );
    files.insert(
        directory.join("src/lib.rs"),
        source(&quote! {
            #![doc = " Generated by progeny. Edits are overwritten on the next run."]
            pub use #types_crate::types;
            #[doc(hidden)]
            mod support;
            pub mod server;
        }),
    );
    files.insert(directory.join("src/server.rs"), source(body));
    files.insert(
        directory.join("src/support.rs"),
        source(&crate::support::server_tokens(
            config.body_limit,
            server_use(api),
        )),
    );
}

fn client_use(api: &ApiModel) -> crate::support::ClientUse {
    let mut used = crate::support::ClientUse::default();
    for operation in api.operations() {
        match &operation.body {
            Some(crate::api::BodyContract::Form { specs, .. }) => {
                used.form = true;
                used.styles.form = true;
                for spec in specs {
                    note_style(&mut used.styles, spec.style);
                }
            }
            Some(crate::api::BodyContract::Multipart { parts, .. }) => {
                used.multipart = true;
                used.parts.text = true;
                used.parts.json = true;
                for part in parts {
                    note_part(&mut used.parts, part.kind);
                }
            }
            Some(
                crate::api::BodyContract::Json { .. }
                | crate::api::BodyContract::Text { .. }
                | crate::api::BodyContract::Bytes { .. },
            )
            | None => {}
        }
        for arm in operation
            .responses
            .arms
            .iter()
            .chain(operation.responses.default.iter())
        {
            match arm.body {
                crate::api::ResponseBody::Text { .. } => used.text_response = true,
                crate::api::ResponseBody::Bytes { .. } => used.byte_response = true,
                crate::api::ResponseBody::Json { .. } | crate::api::ResponseBody::Empty => {}
            }
        }
        for parameter in &operation.params {
            note_style(&mut used.styles, parameter.style.style());
            match parameter.style.location() {
                crate::api::Location::Path
                    if parameter.style.style() == crate::api::Style::Matrix =>
                {
                    used.matrix = true;
                }
                crate::api::Location::Path => used.path = true,
                crate::api::Location::Query => {
                    if parameter.style.allow_reserved() {
                        used.reserved = true;
                    } else {
                        used.query = true;
                    }
                }
                crate::api::Location::Header => used.header = true,
                crate::api::Location::Cookie => used.cookie = true,
            }
        }
    }
    used
}

fn server_use(api: &ApiModel) -> crate::support::ServerUse {
    let mut used = crate::support::ServerUse::default();
    for operation in api
        .operations()
        .iter()
        .filter(|operation| operation.registrable.is_some())
    {
        if operation.body.is_some()
            && super::render::client::content_type_param(operation).is_none()
        {
            used.media_type_gate = true;
        }
        match &operation.body {
            Some(crate::api::BodyContract::Json { .. }) => used.json = true,
            Some(crate::api::BodyContract::Form { specs, .. }) => {
                used.form = true;
                used.styles.form = true;
                for spec in specs {
                    note_style(&mut used.styles, spec.style);
                }
            }
            Some(crate::api::BodyContract::Multipart { parts, .. }) => {
                used.multipart = true;
                used.parts.text = true;
                for part in parts {
                    note_part(&mut used.parts, part.kind);
                }
            }
            Some(crate::api::BodyContract::Text { .. }) => used.text = true,
            Some(crate::api::BodyContract::Bytes { .. }) => used.bytes = true,
            None => {}
        }
        for parameter in &operation.params {
            note_style(&mut used.styles, parameter.style.style());
            match parameter.style.location() {
                crate::api::Location::Path => used.path = true,
                crate::api::Location::Query => used.query = true,
                crate::api::Location::Header => used.header = true,
                crate::api::Location::Cookie => used.cookie = true,
            }
        }
        for arm in operation
            .responses
            .arms
            .iter()
            .chain(operation.responses.default.iter())
        {
            match arm.body {
                crate::api::ResponseBody::Text { .. } => used.text_response = true,
                crate::api::ResponseBody::Bytes { .. } => used.byte_response = true,
                crate::api::ResponseBody::Json { .. } | crate::api::ResponseBody::Empty => {}
            }
        }
    }
    used
}

fn note_style(used: &mut crate::support::StyleUse, style: crate::api::Style) {
    match style {
        crate::api::Style::Form => used.form = true,
        crate::api::Style::Simple => used.simple = true,
        crate::api::Style::Label => used.label = true,
        crate::api::Style::Matrix => used.matrix = true,
        crate::api::Style::SpaceDelimited => used.space_delimited = true,
        crate::api::Style::PipeDelimited => used.pipe_delimited = true,
        crate::api::Style::DeepObject => used.deep_object = true,
    }
}

fn note_part(used: &mut crate::support::PartUse, kind: crate::api::PartKind) {
    match kind {
        crate::api::PartKind::Text => used.text = true,
        crate::api::PartKind::File => used.file = true,
        crate::api::PartKind::Json => used.json = true,
    }
}

/// Format a token stream as source text.
///
/// `prettyplease` rather than rustfmt: a formatter that runs in-process needs no toolchain at
/// generation time and cannot be missing. A stream that does not parse as a file falls back to the
/// token text, which is still valid Rust and still compiles — a formatting failure must not be able
/// to fail a build.
fn source(tokens: &TokenStream) -> String {
    match syn::parse2::<syn::File>(tokens.clone()) {
        Ok(file) => prettyplease::unparse(&file),
        Err(_) => tokens.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use crate::config::{Config, Packaging, SerdeImpl};
    use crate::diag::Ctx;
    use crate::doc::parse as doc_parse;
    use crate::{contract, normalize, resolve, shape};

    /// Run the whole pipeline and return every rendered file.
    fn files(
        document: Value,
        config: &Config,
    ) -> eyre::Result<std::collections::BTreeMap<camino::Utf8PathBuf, String>> {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, config, &mut ctx)?;
        let api = crate::api::build(&resolved, &shapes, &contracts, config, &mut ctx)?;
        Ok(super::run(&contracts, &api, config))
    }

    /// Generate the types module for a document.
    fn types(document: Value, config: &Config) -> eyre::Result<String> {
        Ok(files(document, config)?
            .get(camino::Utf8Path::new("src/types.rs"))
            .cloned()
            .unwrap_or_default())
    }

    /// Generate the client module for a document.
    fn client(document: Value) -> eyre::Result<String> {
        let rendered = files(document, &Config::default())?
            .get(camino::Utf8Path::new("src/client.rs"))
            .cloned()
            .unwrap_or_default();
        // The renderer falls back to unformatted tokens when a stream does not parse, which is the
        // right call for a build and the wrong one for a test that then greps the text.
        syn::parse_file(&rendered)?;
        Ok(rendered)
    }

    /// The escape hatch, named rather than defaulted to.
    ///
    /// The serde attributes only exist in this mode: they are helper attributes of the derive
    /// macros, so a type on the hand-written path carries none at all. A test about how an
    /// attribute renders has to say which mode it is about, and these did not until the default
    /// changed under them.
    fn derive_mode() -> Config {
        Config {
            serde_impl: crate::config::SerdeImpl::DeriveAlways,
            ..Config::default()
        }
    }

    fn with_schemas(schemas: Value) -> Value {
        // Built member by member rather than with `json!`, which would only borrow `schemas`.
        let mut components = serde_json::Map::new();
        components.insert("schemas".to_owned(), schemas);
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), Value::Object(serde_json::Map::new()));
        root.insert("components".to_owned(), Value::Object(components));
        Value::Object(root)
    }

    #[test_util::test]
    fn a_struct_renders_with_its_wire_names_and_its_docs() {
        let rendered = types(
            with_schemas(json!({
                "Pet": {
                    "type": "object",
                    "description": "One pet.",
                    "required": ["petName"],
                    "properties": {
                        "petName": {"type": "string", "description": "What it answers to."},
                        "type": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                    },
                },
            })),
            &derive_mode(),
        )?;
        assert!(rendered.contains("pub struct Pet {"), "{rendered}");
        assert!(rendered.contains("/// One pet."), "{rendered}");
        assert!(rendered.contains("/// What it answers to."), "{rendered}");
        // Case conversion on the Rust side, the wire name kept exactly.
        assert!(
            rendered.contains(r#"#[serde(rename = "petName")]"#),
            "{rendered}"
        );
        assert!(rendered.contains("pub pet_name: String"), "{rendered}");
        // A reserved word is renamed with a suffix, and says what it was.
        assert!(rendered.contains(r#"rename = "type""#), "{rendered}");
        assert!(rendered.contains("pub type_: Option<String>"), "{rendered}");
        assert!(
            rendered.contains(r#"skip_serializing_if = "Option::is_none""#),
            "{rendered}"
        );
        assert!(
            rendered.contains("pub tags: Option<Vec<String>>"),
            "{rendered}"
        );
    }

    /// A type on the hand-written path carries **no** `#[serde(...)]` attributes at all.
    ///
    /// Mandatory rather than tidy, and in two directions. They are helper attributes of the serde
    /// derive macros, so with no derive on the item they do not resolve and the crate does not
    /// compile. And any *other* derive that reads them would then see a different type than the wire
    /// format has — `schemars::JsonSchema` honours `rename` and `skip_serializing_if`, so a schema
    /// generated beside a hand-written impl would disagree with the bytes. That is the forbidden
    /// failure mode, and it is why `DeriveSet` is a closed set of attribute-blind derives.
    ///
    /// The wire name has to survive the move: it stops being an attribute and becomes a string in
    /// the generated impl, and this checks it is still there rather than merely gone from the type.
    #[test_util::test]
    fn a_hand_written_type_carries_no_serde_attributes_and_keeps_its_wire_name() {
        let document = with_schemas(json!({
            "Pet": {
                "type": "object",
                "required": ["petName"],
                "properties": {
                    "petName": {"type": "string"},
                    "type": {"type": "string"},
                },
            },
        }));
        let rendered = types(document, &Config::default())?;
        assert!(rendered.contains("pub struct Pet {"), "{rendered}");
        assert!(
            !rendered.contains("#[serde("),
            "{}",
            indoc::formatdoc! {"
                a hand-written type kept a serde attribute:
                {rendered}"
            }
        );
        assert!(
            !rendered.contains("derive(serde::Deserialize"),
            "{}",
            indoc::formatdoc! {"
                a hand-written type kept the derive:
                {rendered}"
            }
        );
        // The wire names moved into the impl rather than being lost with the attributes.
        assert!(rendered.contains(r#""petName""#), "{rendered}");
        assert!(rendered.contains("pub pet_name: String"), "{rendered}");
        assert!(rendered.contains("pub type_: Option<String>"), "{rendered}");
    }

    #[test_util::test]
    fn the_rendered_source_parses_as_rust() {
        // Formatting is what proves the stream is syntactically valid: `prettyplease` only runs on
        // a `syn::File`, and the fallback would leave the tokens on one line.
        let rendered = types(
            with_schemas(json!({
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            })),
            &Config::default(),
        )?;
        syn::parse_file(&rendered)?;
        assert!(rendered.lines().count() > 3, "{rendered}");
    }

    #[test_util::test]
    fn a_workspace_has_three_feature_isolated_crates() {
        let config = Config {
            packaging: Packaging::Workspace,
            ..Config::default()
        };
        let rendered = files(
            json!({
                "openapi": "3.1.0",
                "info": {"title": "pets", "version": "1"},
                "paths": {
                    "/pets": {
                        "get": {
                            "operationId": "listPets",
                            "responses": {
                                "200": {
                                    "description": "pets",
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "type": "array",
                                                "items": {"$ref": "#/components/schemas/Pet"}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": {
                        "Pet": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}}
                        }
                    }
                }
            }),
            &config,
        )?;

        let root = rendered
            .get(camino::Utf8Path::new("Cargo.toml"))
            .ok_or_eyre("workspace root manifest")?;
        assert!(root.contains(r#"members = ["api-types", "api-client", "api-server"]"#));

        let types_manifest = rendered
            .get(camino::Utf8Path::new("api-types/Cargo.toml"))
            .ok_or_eyre("types manifest")?;
        assert!(types_manifest.contains(r#"name = "api-types""#));
        assert!(!types_manifest.contains("[features]"), "{types_manifest}");

        for half in ["client", "server"] {
            let path = format!("api-{half}/Cargo.toml");
            let manifest = rendered
                .get(camino::Utf8Path::new(&path))
                .ok_or_eyre(path)?;
            assert!(
                manifest.contains(r#"api-types = { path = "../api-types", version = "=0.1.0" }"#),
                "{manifest}"
            );
            let path = format!("api-{half}/src/lib.rs");
            let root = rendered
                .get(camino::Utf8Path::new(&path))
                .ok_or_eyre(path)?;
            assert!(root.contains("pub use api_types::types;"), "{root}");
        }

        let types_support = rendered
            .get(camino::Utf8Path::new("api-types/src/support.rs"))
            .ok_or_eyre("types support")?;
        assert!(types_support.contains("BufferVisitor"), "{types_support}");
        assert!(!types_support.contains("ResponseValue"), "{types_support}");
        assert!(!types_support.contains("Rejection"), "{types_support}");

        let client_support = rendered
            .get(camino::Utf8Path::new("api-client/src/support.rs"))
            .ok_or_eyre("client support")?;
        assert!(client_support.contains("ResponseValue"), "{client_support}");
        assert!(
            !client_support.contains("BufferVisitor"),
            "{client_support}"
        );
        assert!(!client_support.contains("Rejection"), "{client_support}");

        let server_support = rendered
            .get(camino::Utf8Path::new("api-server/src/support.rs"))
            .ok_or_eyre("server support")?;
        assert!(server_support.contains("Rejection"), "{server_support}");
        assert!(
            !server_support.contains("BufferVisitor"),
            "{server_support}"
        );
        assert!(
            !server_support.contains("ResponseValue"),
            "{server_support}"
        );

        let readme = rendered
            .get(camino::Utf8Path::new("README.md"))
            .ok_or_eyre("workspace README")?;
        assert!(
            readme.contains("publish `api-types` before `api-client` and `api-server`"),
            "{readme}"
        );
    }

    #[test_util::test]
    fn a_fieldless_enum_keeps_the_bytes_it_had() {
        let rendered = types(
            with_schemas(json!({
                "State": {"type": "string", "enum": ["in-progress", "done", "2fa"]},
            })),
            &derive_mode(),
        )?;
        assert!(rendered.contains("pub enum State"), "{rendered}");
        assert!(
            rendered.contains(r#"#[serde(rename = "in-progress")]"#),
            "{rendered}"
        );
        assert!(rendered.contains("InProgress,"), "{rendered}");
        assert!(rendered.contains("_2fa,"), "{rendered}");
    }

    #[test_util::test]
    fn a_union_renders_untagged() {
        let rendered = types(
            with_schemas(json!({
                "Either": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
            })),
            &Config::default(),
        )?;
        assert!(rendered.contains("#[serde(untagged)]"), "{rendered}");
        assert!(rendered.contains("Text(Box<String>)"), "{rendered}");
        assert!(rendered.contains("Integer(Box<i64>)"), "{rendered}");
    }

    #[test_util::test]
    fn a_union_that_shape_cannot_decide_renders_its_tag() {
        let rendered = types(
            with_schemas(json!({
                "Cat": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
                "Dog": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
                "Animal": {
                    "oneOf": [
                        {"$ref": "#/components/schemas/Cat"},
                        {"$ref": "#/components/schemas/Dog"},
                    ],
                    "discriminator": {"propertyName": "kind", "mapping": {"a cat": "#/components/schemas/Cat"}},
                },
            })),
            &Config::default(),
        )?;
        assert!(rendered.contains(r#"#[serde(tag = "kind")]"#), "{rendered}");
        assert!(rendered.contains("pub enum Animal"), "{rendered}");
        // The mapped name where the document gave one, the component name where it did not.
        assert!(
            rendered.contains(r#"#[serde(rename = "a cat")]"#),
            "{rendered}"
        );
        assert!(rendered.contains("ACat(Box<Cat>)"), "{rendered}");
        assert!(rendered.contains("Dog(Box<Dog>)"), "{rendered}");
        // And the property serde consumes is gone from the variants, because a variant that still
        // declared it would be handed a payload the tag had already been taken out of.
        assert!(!rendered.contains("pub kind:"), "{rendered}");
        assert!(
            rendered.contains("pub shared: Option<String>"),
            "{rendered}"
        );
    }

    /// The accessor exists under both serde configurations, and always answers the document's
    /// wire value — the mapping a caller otherwise recovers by serializing into `serde_json`.
    #[test_util::test]
    fn a_string_enum_carries_an_accessor_for_its_wire_values() {
        let document = with_schemas(json!({
            "Status": {"type": "string", "enum": ["OPENED", "IN_CONSULTATION"]},
        }));
        for config in [Config::default(), derive_mode()] {
            let rendered = types(document.clone(), &config)?;
            assert!(
                rendered.contains("pub fn as_str(&self) -> &'static str"),
                "{rendered}"
            );
            assert!(
                rendered.contains(r#"Self::Opened => "OPENED""#),
                "{rendered}"
            );
            assert!(
                rendered.contains(r#"Self::InConsultation => "IN_CONSULTATION""#),
                "{rendered}"
            );
        }
    }

    /// A multipart body's own binary member is a file part, and a file part is bytes plus the
    /// metadata — filename, its own content type — that no `String` carries.
    #[test_util::test]
    fn a_multipart_body_member_declared_binary_is_an_upload() {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/dokumente": {
                    "post": {
                        "operationId": "uploadDocument",
                        "requestBody": {
                            "required": true,
                            "content": {
                                "multipart/form-data": {"schema": {"type": "object", "properties": {
                                    "file": {"type": "string", "format": "binary"},
                                    "anzeigename": {"type": "string"},
                                }}},
                            },
                        },
                        "responses": {"201": {"description": "made"}},
                    },
                },
            },
        });
        for config in [Config::default(), derive_mode()] {
            let rendered = types(document.clone(), &config)?;
            assert!(
                rendered.contains("pub file: Option<super::support::Upload>"),
                "{rendered}"
            );
            assert!(
                rendered.contains("pub anzeigename: Option<String>"),
                "{rendered}"
            );
        }
        // The support type itself ships beside the types module, whichever way the crate is
        // emitted.
        let support = files(document, &Config::default())?
            .get(camino::Utf8Path::new("src/support.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(support.contains("pub struct Upload"), "{support}");
    }

    #[test_util::test]
    fn a_schema_on_two_wires_keeps_the_json_spelling_and_says_so() {
        // The same schema as a multipart body and a JSON body: one schema is one Rust type, so
        // retyping its binary member to `Upload` would put the upload object form on the JSON
        // wire. The conflict keeps `String` everywhere and degrades the multipart side out loud.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/form": {"post": {
                    "operationId": "sendForm",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {"$ref": "#/components/schemas/Payload"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/json": {"post": {
                    "operationId": "sendJson",
                    "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Payload"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
            "components": {"schemas": {
                "Payload": {"type": "object", "properties": {
                    "file": {"type": "string", "contentMediaType": "application/octet-stream"},
                }},
            }},
        });
        let mut ctx = Ctx::new();
        let config = Config::default();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx)?;
        let api = crate::api::build(&resolved, &shapes, &contracts, &config, &mut ctx)?;
        let rendered = super::run(&contracts, &api, &config)
            .get(camino::Utf8Path::new("src/types.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(rendered.contains("pub file: Option<String>"), "{rendered}");
        assert!(!rendered.contains("Upload"), "{rendered}");
        let diagnostics = ctx.into_diagnostics();
        let collapse = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.detail().contains("keep the JSON spelling"))
            .ok_or_eyre("the collapse is reported")?;
        assert!(
            collapse.location().to_string().contains("~1form"),
            "{collapse:?}"
        );
    }

    #[test_util::test]
    fn a_schema_shared_with_a_response_header_keeps_the_json_spelling() {
        // A response header's schema is a named-type position even though no renderer writes
        // headers yet; retyping its binary member for the multipart side would change the
        // header type's shape with nothing saying so.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/form": {"post": {
                    "operationId": "sendForm",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {"$ref": "#/components/schemas/Attachment"}}}},
                    "responses": {"204": {
                        "description": "done",
                        "headers": {"X-Echo": {"schema": {"$ref": "#/components/schemas/Attachment"}}},
                    }},
                }},
            },
            "components": {"schemas": {
                "Attachment": {"type": "object", "properties": {
                    "file": {"type": "string", "format": "binary"},
                }},
            }},
        });
        let mut ctx = Ctx::new();
        let config = Config::default();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx)?;
        let api = crate::api::build(&resolved, &shapes, &contracts, &config, &mut ctx)?;
        let rendered = super::run(&contracts, &api, &config)
            .get(camino::Utf8Path::new("src/types.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(!rendered.contains("Upload"), "{rendered}");
        let diagnostics = ctx.into_diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("keep the JSON spelling")),
            "{diagnostics:?}"
        );
    }

    #[test_util::test]
    fn a_declared_content_type_parameter_outranks_the_bodys_own() {
        // cloudflare's R2 object upload, in miniature: an octet-stream body *and* a declared
        // optional `Content-Type` header — the stored object's type is the caller's choice.
        // The body's automatic header has to yield when the parameter is set, and the server's
        // 415 gate has to stand down entirely: the description made the type caller data.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/objects/{key}": {"put": {
                    "operationId": "putObject",
                    "parameters": [
                        {"name": "key", "in": "path", "required": true, "schema": {"type": "string"}},
                        {"name": "Content-Type", "in": "header", "schema": {"type": "string"}},
                    ],
                    "requestBody": {"required": true, "content": {"application/octet-stream": {
                        "schema": {"type": "string", "contentMediaType": "application/octet-stream"},
                    }}},
                    "responses": {"204": {"description": "stored"}},
                }},
            },
        });
        // Workspace packaging: the per-edge crates are where the dead-code surgery runs, and
        // they are what `task corpus:compile` checks under `-D warnings`.
        let config = Config {
            packaging: crate::config::Packaging::Workspace,
            ..Config::default()
        };
        let rendered = files(document, &config)?;
        let client = &rendered[camino::Utf8Path::new("api-client/src/client.rs")];
        syn::parse_file(client)?;
        assert!(
            client.contains("let mut declared_content_type = false;"),
            "{client}"
        );
        assert!(client.contains("declared_content_type = true;"), "{client}");
        assert!(client.contains("if !declared_content_type {"), "{client}");
        let server = &rendered[camino::Utf8Path::new("api-server/src/server.rs")];
        assert!(!server.contains("mismatched_media_type"), "{server}");
        // With every body op gated off, the shipped reader method is expected-dead rather
        // than a `-D warnings` failure in the generated crate.
        let support = &rendered[camino::Utf8Path::new("api-server/src/support.rs")];
        let at = support
            .find("fn mismatched_media_type")
            .ok_or_eyre("the method ships")?;
        let above = &support[at.saturating_sub(2000)..at];
        assert!(above.contains("expect(dead_code"), "{above}");
    }

    #[test_util::test]
    fn a_path_value_that_spells_a_dot_segment_is_refused_before_the_request_exists() {
        // `id = ".."` would render `/files/../metadata`, which reqwest's URL parser folds to
        // `/metadata` — another endpoint. The segment is assembled apart and checked.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/files/{id}/metadata": {"get": {
                    "operationId": "fileMetadata",
                    "parameters": [
                        {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
                    ],
                    "responses": {"204": {"description": "done"}},
                }},
            },
        });
        let rendered = client(document)?;
        assert!(rendered.contains("Error::UnsendablePath"), "{rendered}");
        assert!(
            rendered.contains(r#"matches!(segment.as_str(), "." | "..")"#),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn an_allow_reserved_query_value_routes_around_reqwests_encoder() {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/jump": {"get": {
                    "operationId": "jump",
                    "parameters": [
                        {"name": "to", "in": "query", "required": true,
                         "allowReserved": true, "schema": {"type": "string"}},
                        {"name": "tag", "in": "query", "schema": {"type": "string"}},
                    ],
                    "responses": {"204": {"description": "done"}},
                }},
            },
        });
        let rendered = client(document)?;
        // The reserved pair is appended to the URL itself; the ordinary one still goes through
        // `.query(...)`.
        assert!(
            rendered.contains("support::style::reserved_pair(name, value)"),
            "{rendered}"
        );
        assert!(rendered.contains("request.query(&query)"), "{rendered}");
    }

    #[test_util::test]
    fn an_unset_declared_accept_falls_back_to_the_configured_one() {
        // The declared optional `Accept` parameter outranks the configured media types only
        // when the caller sets it; unset, the configuration's argument still has to reach the
        // wire. Suppressing on the mere declaration left the server free to answer with the
        // very type the configuration refused.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/preview": {"get": {
                    "operationId": "getPreview",
                    "parameters": [
                        {"name": "Accept", "in": "header", "schema": {"type": "string"}},
                    ],
                    "responses": {"200": {
                        "description": "ok",
                        "content": {
                            "application/json": {"schema": {"type": "object"}},
                            "image/jpeg": {"schema": {"type": "string", "format": "binary"}},
                        },
                    }},
                }},
            },
        });
        let config = Config {
            media_types: [(
                "/paths/~1preview/get/responses/200/content".to_owned(),
                "image/jpeg".to_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let rendered = files(document, &config)?
            .get(camino::Utf8Path::new("src/client.rs"))
            .cloned()
            .unwrap_or_default();
        syn::parse_file(&rendered)?;
        assert!(rendered.contains("} else {"), "{rendered}");
        assert!(
            rendered.contains(r#"request.header(::reqwest::header::ACCEPT, "image/jpeg")"#),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_wrapped_multipart_body_still_gets_its_upload() {
        // A single-branch `oneOf` classifies as an alias of its target, and the target's key is
        // the one the struct lowers under. The retype has to follow the wrapper to that core:
        // keyed on the wrapper itself, the binary member silently stayed a text part.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/upload": {"post": {
                    "operationId": "upload",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {
                        "oneOf": [{"$ref": "#/components/schemas/UploadForm"}],
                    }}}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
            "components": {"schemas": {
                "UploadForm": {"type": "object", "properties": {
                    "file": {"type": "string", "format": "binary"},
                }},
            }},
        });
        let rendered = types(document, &Config::default())?;
        assert!(
            rendered.contains("pub file: Option<super::support::Upload>"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_wrapper_beside_a_direct_multipart_use_is_no_conflict() {
        // Both operations put the same schema on the same kind of wire — a multipart request
        // body. Seeding the wrapper's children marked the target as reachable from a foreign
        // position and degraded the direct use with a reason that named a wire that does not
        // exist.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/upload-direct": {"post": {
                    "operationId": "uploadDirect",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {"$ref": "#/components/schemas/Form"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/upload-wrapped": {"post": {
                    "operationId": "uploadWrapped",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {
                        "oneOf": [{"$ref": "#/components/schemas/Form"}],
                    }}}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
            "components": {"schemas": {
                "Form": {"type": "object", "properties": {
                    "file": {"type": "string", "format": "binary"},
                }},
            }},
        });
        let mut ctx = Ctx::new();
        let config = Config::default();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx)?;
        let api = crate::api::build(&resolved, &shapes, &contracts, &config, &mut ctx)?;
        let rendered = super::run(&contracts, &api, &config)
            .get(camino::Utf8Path::new("src/types.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(
            rendered.contains("pub file: Option<super::support::Upload>"),
            "{rendered}"
        );
        let diagnostics = ctx.into_diagnostics();
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("keep the JSON spelling")),
            "{diagnostics:?}"
        );
    }

    #[test_util::test]
    fn a_named_binary_alias_member_is_still_a_file_part() {
        // The member's `$ref` lands on a named component, so its lowered type is the
        // component's bare name. The file-part decision is the shape's: judged on the type
        // alone, the part silently became UTF-8 text.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/upload": {"post": {
                    "operationId": "upload",
                    "requestBody": {"content": {"multipart/form-data": {"schema": {"$ref": "#/components/schemas/Form"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
            "components": {"schemas": {
                "Form": {"type": "object", "properties": {
                    "file": {"$ref": "#/components/schemas/BinaryFile"},
                }},
                "BinaryFile": {"type": "string", "format": "binary"},
            }},
        });
        let rendered = types(document, &Config::default())?;
        assert!(
            rendered.contains("pub file: Option<super::support::Upload>"),
            "{rendered}"
        );
    }

    /// The carried style is hand-written under *both* serde configurations: no derive encoding
    /// reads a tag it leaves in the payload, so `DeriveAlways` has nothing to offer this kind.
    #[test_util::test]
    fn a_union_that_cannot_consume_its_tag_renders_a_carried_tag_enum() {
        let document = with_schemas(json!({
            "A": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "B": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
            "Either": {
                "oneOf": [
                    {"$ref": "#/components/schemas/A"},
                    {"$ref": "#/components/schemas/B"},
                ],
                "discriminator": {"propertyName": "kind"},
            },
            // The use that keeps `kind` on `A`, and the tag on the variants.
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        }));
        for config in [Config::default(), derive_mode()] {
            let rendered = types(document.clone(), &config)?;
            assert!(rendered.contains("pub enum Either {"), "{rendered}");
            // Dispatch reads the tag and replays the payload; the enum item itself carries no
            // serde surface at all.
            assert!(rendered.contains("support::dispatch"), "{rendered}");
            assert!(
                rendered.contains(r#"const TAG: &str = "kind";"#),
                "{rendered}"
            );
            assert!(
                rendered.contains(r#"const VARIANTS: &[&str] = &["A", "B"];"#),
                "{rendered}"
            );
            assert!(!rendered.contains("serde(tag"), "{rendered}");
            assert!(!rendered.contains("serde(untagged)"), "{rendered}");
            // The variant type keeps the member the tag rides in.
            assert!(rendered.contains("pub kind: Option<String>"), "{rendered}");
        }
    }

    /// Two tagged unions that agree on everything but the bytes their tags read are two types.
    ///
    /// The trap is that a Rust variant name is a *normalization* of the tag value — `a-cat` and
    /// `aCat` both name an `ACat` — so an identity built from names and types alone calls these
    /// unions equal, merges them, and hands one document position the other's wire bytes. The tag
    /// values are part of the dedup fingerprint precisely so this cannot happen.
    #[test_util::test]
    fn two_tagged_unions_that_differ_only_in_their_tag_bytes_stay_two_types() {
        let body = |mapping: Value| {
            json!({"post": {
                "requestBody": {"content": {"application/json": {"schema": {
                    "oneOf": [
                        {"$ref": "#/components/schemas/Cat"},
                        {"$ref": "#/components/schemas/Dog"},
                    ],
                    "discriminator": {"propertyName": "kind", "mapping": mapping},
                }}}},
                "responses": {"204": {"description": "done"}},
            }})
        };
        let rendered = types(
            json!({
                "openapi": "3.1.0",
                "components": {"schemas": {
                    "Cat": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
                    "Dog": {"type": "object", "properties": {"kind": {"type": "string"}, "shared": {"type": "string"}}},
                }},
                "paths": {
                    "/one": body(json!({
                        "a-cat": "#/components/schemas/Cat",
                        "a-dog": "#/components/schemas/Dog",
                    })),
                    "/two": body(json!({
                        "aCat": "#/components/schemas/Cat",
                        "aDog": "#/components/schemas/Dog",
                    })),
                },
            }),
            &derive_mode(),
        )?;
        assert_eq!(
            rendered.matches(r#"#[serde(tag = "kind")]"#).count(),
            2,
            "{}",
            indoc::formatdoc! {"
                the two unions merged:
                {rendered}"
            }
        );
        // Each position keeps the exact bytes its own mapping declared.
        assert!(
            rendered.contains(r#"#[serde(rename = "a-cat")]"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"#[serde(rename = "aCat")]"#),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_declared_default_is_documented_rather_than_applied_on_the_way_in() {
        let rendered = types(
            with_schemas(json!({
                "Options": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "default": 20},
                        "name": {"type": "string", "default": "unnamed"},
                    },
                },
            })),
            &Config::default(),
        )?;
        // `#[serde(default = "…")]` would fill the member in on the way *in*, and it would then be
        // written on the way *out* — turning "the caller said nothing" into "the caller said 20" on
        // every request. The payload gate caught exactly that over the corpus.
        assert!(!rendered.contains("default = "), "{rendered}");
        assert!(!rendered.contains("fn default_options_limit"), "{rendered}");
        // Nothing is lost quietly: the server's assumption is stated where a reader will find it.
        assert!(
            rendered.contains("The server assumes `20` when this member is absent."),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"The server assumes `"unnamed"` when this member is absent."#),
            "{rendered}"
        );
        // And an absent member still deserializes, because every optional field is an `Option`.
        assert!(rendered.contains("pub limit: Option<i64>"), "{rendered}");
    }

    #[test_util::test]
    fn the_hand_written_strategy_leaves_no_serde_attributes_behind() {
        // Attributes are helper attributes of the serde derive macros: with no derive on the item
        // they do not even resolve, so leaving one behind would not compile.
        let config = Config {
            serde_impl: SerdeImpl::HandWrittenWhereEligible,
            ..Config::default()
        };
        let rendered = types(
            with_schemas(json!({
                "Pet": {"type": "object", "properties": {"petName": {"type": "string"}}},
            })),
            &config,
        )?;
        assert!(!rendered.contains("#[serde("), "{rendered}");
        assert!(!rendered.contains("#[derive(serde::"), "{rendered}");
        assert!(
            rendered.contains("pub pet_name: Option<String>"),
            "{rendered}"
        );
        // Two function bodies per type, calling into the machinery that lives once per crate.
        assert!(
            rendered.contains("impl<'de> serde::Deserialize<'de> for Pet"),
            "{rendered}"
        );
        assert!(
            rendered.contains("impl serde::Serialize for Pet"),
            "{rendered}"
        );
        assert!(
            rendered.contains("super::support::BufferVisitor"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn crate_mode_and_module_mode_render_the_same_types() {
        let document = with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
        }));
        let as_crate = types(document.clone(), &Config::default())?;
        let module = Config {
            packaging: Packaging::Module,
            ..Config::default()
        };
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &module, &mut ctx)?;
        let api = crate::api::build(&resolved, &shapes, &contracts, &module, &mut ctx)?;
        let files = super::run(&contracts, &api, &module);
        assert_eq!(files.len(), 1);
        let nested = files
            .get(camino::Utf8Path::new("progeny.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(nested.contains("pub mod types"), "{nested}");
        // The same struct, indented into a module: one renderer behind both packagings.
        assert!(nested.contains("pub struct Pet"), "{nested}");
        assert!(as_crate.contains("pub struct Pet"), "{as_crate}");
        // The file is `include!`d, and an inner attribute expanded from a macro annotates nothing
        // it is allowed to — so the notice that the file is generated is a line comment, and
        // nothing above the first item is inner. Inner attributes *inside* the nested modules are
        // fine and the support module carries one.
        let preamble = nested.split("pub mod").next().unwrap_or_default();
        assert_eq!(
            preamble.trim(),
            "// Generated by progeny. Edits are overwritten on the next run."
        );
    }

    #[test_util::test]
    fn a_path_parameter_is_a_plain_field_filled_into_the_url() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets/{petId}": {"get": {
                "operationId": "showPetById",
                "parameters": [{"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}}],
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains("pub fn show_pet_by_id(&self, params: ShowPetByIdParams)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("pub struct ShowPetByIdRequest<'a>"),
            "{rendered}"
        );
        // Required, so a plain field — never an `Option`, never a check at `send`.
        assert!(rendered.contains("pub pet_id: String"), "{rendered}");
        // The separator, the literal segment, the separator, the variable.
        assert!(rendered.contains(r"url.push('/')"), "{rendered}");
        assert!(rendered.contains(r#"url.push_str("pets")"#), "{rendered}");
        assert!(
            rendered.contains("support::style::path_segment"),
            "{rendered}"
        );
        assert!(
            rendered.contains("let value = &self.params.pet_id;"),
            "{rendered}"
        );
        // Whatever the description requires existed when the params struct was built, so nothing
        // in the client can refuse at runtime.
        assert!(!rendered.contains("Unset"), "{rendered}");
    }

    #[test_util::test]
    fn an_optional_input_is_an_option_field_and_a_required_body_a_plain_one() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"post": {
                "operationId": "createPet",
                "parameters": [{"name": "verbose", "in": "query", "schema": {"type": "boolean"}}],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}},
                "responses": {"200": {"description": "ok"}},
            }}},
            "components": {"schemas": {"Pet": {"type": "object", "properties": {"name": {"type": "string"}}}}},
        }))?;
        assert!(
            rendered.contains("pub verbose: ::std::option::Option<bool>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("pub body: super::types::Pet"),
            "{rendered}"
        );
        // A required body reads directly; only an optional input contributes conditionally.
        assert!(
            rendered.contains("let body = &self.params.body;"),
            "{rendered}"
        );
        assert!(
            rendered.contains("if let ::std::option::Option::Some(value) = &self.params.verbose"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_body_the_document_does_not_require_is_an_option_field() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"post": {
                "operationId": "createPet",
                "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}},
                "responses": {"200": {"description": "ok"}},
            }}},
            "components": {"schemas": {"Pet": {"type": "object", "properties": {"name": {"type": "string"}}}}},
        }))?;
        assert!(
            rendered.contains("pub body: ::std::option::Option<super::types::Pet>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("if let ::std::option::Option::Some(body) = &self.params.body"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn an_operation_with_no_input_takes_no_params_struct() {
        // An accessor with no argument states "no input" better than an empty literal would, and
        // gaining a first input still changes the accessor's signature, so drift stays a compile
        // error at every call site.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains("pub fn list_pets(&self) -> ListPetsRequest<'_>"),
            "{rendered}"
        );
        assert!(!rendered.contains("ListPetsParams"), "{rendered}");
    }

    #[test_util::test]
    fn an_undeclared_header_rides_the_request_and_replaces_a_declared_one() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains(
                "pub fn header(
        mut self,
        name: ::reqwest::header::HeaderName,
        value: ::reqwest::header::HeaderValue,
    ) -> Self"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("request = request.headers(self.headers)"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_deprecated_parameter_on_a_live_operation_does_not_warn_at_the_call_site() {
        // Every literal is obliged to name every params field, so a field-level `#[deprecated]`
        // would warn at call sites that cannot avoid it — and inside the generated `send` itself.
        // The deprecation rides the field's prose instead.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/artists": {"get": {
                "operationId": "getArtists",
                "parameters": [{"name": "limit", "in": "query", "deprecated": true, "schema": {"type": "integer"}}],
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains(" Deprecated. Sent as the `limit` query parameter."),
            "{rendered}"
        );
        assert!(!rendered.contains("#[deprecated]"), "{rendered}");
        assert!(!rendered.contains("expect(deprecated"), "{rendered}");
    }

    #[test_util::test]
    fn a_vendor_json_media_type_is_the_content_type_the_wire_carries() {
        // reqwest's `json` writes `application/json` only when the header is absent, so a
        // `+json` structured-syntax position must set its declared spelling first — a variant
        // method whose whole point is the other media type must not send the family default.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/things": {"post": {
                "operationId": "createThing",
                "requestBody": {"required": true, "content": {
                    "application/vnd.example.2024-01-01+json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}},
                }},
                "responses": {"204": {"description": "done"}},
            }}},
        }))?;
        assert!(
            rendered.contains(r#""application/vnd.example.2024-01-01+json","#),
            "{rendered}"
        );
        assert!(rendered.contains(".json(body)"), "{rendered}");
        // And a plain `application/json` position keeps the bare call.
        let plain = client(json!({
            "openapi": "3.1.0",
            "paths": {"/things": {"post": {
                "operationId": "createThing",
                "requestBody": {"required": true, "content": {
                    "application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}},
                }},
                "responses": {"204": {"description": "done"}},
            }}},
        }))?;
        assert!(!plain.contains("CONTENT_TYPE"), "{plain}");
    }

    #[test_util::test]
    fn a_paginated_stream_assigns_the_cursor_the_field_actually_has() {
        // The one rendering of `stream()` nothing else compiles: no corpus configuration
        // declares pagination, so this parses the emitted client and pins the assignment for
        // every optionality the cursor parameter can have.
        let paged = |cursor: Value| {
            json!({
                "openapi": "3.1.0",
                "paths": {"/items": {"get": {
                    "operationId": "listItems",
                    "parameters": [cursor],
                    "responses": {"200": {"description": "a page", "content": {"application/json": {"schema": {
                        "type": "object",
                        "required": ["items"],
                        "properties": {
                            "items": {"type": "array", "items": {"type": "string"}},
                            "next_cursor": {"type": "string"},
                        },
                    }}}}},
                }}},
            })
        };
        let config = Config {
            pagination: [(
                "list_items".to_owned(),
                crate::config::Pagination {
                    cursor_param: "cursor".to_owned(),
                    next_cursor: "next_cursor".to_owned(),
                    items: "items".to_owned(),
                },
            )]
            .into(),
            ..Config::default()
        };
        let rendered = |cursor: Value| -> eyre::Result<String> {
            let rendered = files(paged(cursor), &config)?
                .get(camino::Utf8Path::new("src/client.rs"))
                .cloned()
                .unwrap_or_default();
            syn::parse_file(&rendered)?;
            Ok(rendered)
        };

        let optional =
            rendered(json!({"name": "cursor", "in": "query", "schema": {"type": "string"}}))?;
        assert!(
            optional.contains("page_request.params.cursor = ::std::option::Option::Some(cursor);"),
            "{optional}"
        );

        let required = rendered(
            json!({"name": "cursor", "in": "query", "required": true, "schema": {"type": "string"}}),
        )?;
        assert!(
            required.contains("page_request.params.cursor = cursor;"),
            "{required}"
        );

        // Required but nullable: the field keeps the type's own `Option`, so the assignment
        // restores exactly one layer.
        let nullable = rendered(json!({
            "name": "cursor", "in": "query", "required": true,
            "schema": {"type": ["string", "null"]},
        }))?;
        assert!(
            nullable.contains("page_request.params.cursor = ::std::option::Option::Some(cursor);"),
            "{nullable}"
        );
    }

    #[test_util::test]
    fn an_upload_forces_the_support_module_whatever_the_serde_strategy() {
        // Under `derive-always` with no carried-tag union, no hand-written serde needs the
        // support module — but a multipart binary member spells `super::support::Upload` in the
        // types, so the module must ship anyway, in both packagings, publicly enough for a
        // consumer to construct the field.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {"/dokumente": {"post": {
                "operationId": "uploadDocument",
                "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "properties": {"file": {"type": "string", "format": "binary"}},
                }}}},
                "responses": {"201": {"description": "made"}},
            }}},
        });
        let config = Config {
            packaging: Packaging::Workspace,
            serde_impl: crate::config::SerdeImpl::DeriveAlways,
            ..Config::default()
        };
        let generated = files(document.clone(), &config)?;
        let lib = generated
            .get(camino::Utf8Path::new("api-types/src/lib.rs"))
            .cloned()
            .unwrap_or_default();
        assert!(lib.contains("pub mod support;"), "{lib}");
        assert!(
            generated.contains_key(camino::Utf8Path::new("api-types/src/support.rs")),
            "the types crate ships the module its types name"
        );

        let types_only = Config {
            emit: crate::config::Emit {
                types: true,
                client: false,
                server: false,
            },
            serde_impl: crate::config::SerdeImpl::DeriveAlways,
            ..Config::default()
        };
        let generated = files(document, &types_only)?;
        assert!(
            generated.contains_key(camino::Utf8Path::new("src/support.rs")),
            "{:?}",
            generated.keys().collect::<Vec<_>>()
        );
    }

    #[test_util::test]
    fn a_named_type_is_qualified_wherever_it_is_not_the_types_module() {
        // A schema called `Error`: unqualified, this renders `Error<Error>` whose two `Error`s are
        // different types — the support module's and this one — and it compiles.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}},
                    "404": {"description": "gone", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                },
            }}},
            "components": {"schemas": {
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
                "Error": {"type": "object", "properties": {"message": {"type": "string"}}},
            }},
        }))?;
        assert!(
            rendered.contains("ResponseValue<super::types::Pet>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Error<super::types::Error>"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn several_declared_failures_become_an_enum_and_one_does_not() {
        let two = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "404": {"description": "gone"},
                    "429": {"description": "slow down", "content": {"application/json": {"schema": {"type": "string"}}}},
                },
            }}},
        }))?;
        assert!(two.contains("pub enum ListPetsError"), "{two}");
        assert!(two.contains("NotFound(())"), "{two}");
        assert!(two.contains("TooManyRequests(Box<String>)"), "{two}");

        // One failure needs no enum: its payload type is the error type.
        let one = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "404": {"description": "gone", "content": {"application/json": {"schema": {"type": "string"}}}},
                },
            }}},
        }))?;
        assert!(!one.contains("pub enum ListPetsError"), "{one}");
        assert!(one.contains("Error<String>"), "{one}");
    }

    #[test_util::test]
    fn a_range_lower_bound_expects_only_the_overlap_clippy_reports() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "400": {"description": "specific client error"},
                    "4XX": {"description": "other client error"},
                },
            }}},
        }))?;
        assert!(
            rendered.contains("clippy::match_overlapping_arm")
                && rendered.contains(
                    r#"reason = "OpenAPI gives an exact status precedence over its declared status range""#
                ),
            "{rendered}"
        );

        let interior = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "404": {"description": "specific client error"},
                    "4XX": {"description": "other client error"},
                },
            }}},
        }))?;
        assert!(
            !interior.contains("clippy::match_overlapping_arm"),
            "{interior}"
        );
    }

    /// One operation with one request body of the given media type and schema.
    fn body_client(media_type: &str, entry: &Value) -> eyre::Result<String> {
        client(json!({
            "openapi": "3.1.0",
            "paths": {"/upload": {"post": {
                "operationId": "upload",
                "requestBody": {"required": true, "content": {media_type: entry}},
                "responses": {"200": {"description": "ok"}},
            }}},
        }))
    }

    #[test_util::test]
    fn a_form_body_is_encoded_by_the_style_table_rather_than_as_json() {
        let rendered = body_client(
            "application/x-www-form-urlencoded",
            &json!({"schema": {"type": "object", "properties": {"grant_type": {"type": "string"}}}}),
        )?;
        assert!(
            rendered.contains("application/x-www-form-urlencoded"),
            "{rendered}"
        );
        assert!(
            rendered.contains("support::style::form_body("),
            "{rendered}"
        );
        // The body is still the typed struct: a form body differs from a JSON one in how it is
        // written, not in what the caller builds.
        assert!(!rendered.contains("request.json(body)"), "{rendered}");
    }

    #[test_util::test]
    fn a_form_bodys_encoding_reaches_the_generated_table() {
        let rendered = body_client(
            "application/x-www-form-urlencoded",
            &json!({
                "schema": {"type": "object", "properties": {"tag": {"type": "array", "items": {"type": "string"}}}},
                "encoding": {"tag": {"style": "spaceDelimited", "explode": false}},
            }),
        )?;
        assert!(
            rendered.contains("support::style::FormSpec"),
            "the encoding declaration did not reach the emitted table: {rendered}"
        );
        assert!(
            rendered.contains("support::style::Style::SpaceDelimited"),
            "{rendered}"
        );
        assert!(rendered.contains("explode: false"), "{rendered}");
    }

    #[test_util::test]
    fn a_multipart_bodys_parts_come_from_the_contract() {
        let rendered = body_client(
            "multipart/form-data",
            &json!({"schema": {"type": "object", "properties": {
                "file": {"type": "string", "format": "binary"},
                "name": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
            }}}),
        )?;
        assert!(rendered.contains("support::multipart::body("), "{rendered}");
        // One row per member, each classified from the type the member was actually given.
        assert!(
            rendered.contains("name: \"file\"") && rendered.contains("PartKind::File"),
            "{rendered}"
        );
        assert!(
            rendered.contains("name: \"name\"") && rendered.contains("PartKind::Text"),
            "{rendered}"
        );
        // An array is the item's kind, repeated — 3.1's rule. 3.0 would have said one JSON array
        // here, which is not what a server parsing repeated fields is looking for.
        assert!(
            rendered.contains("name: \"tags\"") && rendered.contains("repeated: true"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("PartKind::Text").count(), 2, "{rendered}");
        // The boundary is chosen from the content, so the header cannot be set before the body
        // exists — which is why both come back from one call.
        assert!(rendered.contains("let (content_type, bytes)"), "{rendered}");
    }

    #[test_util::test]
    fn a_multipart_encoding_names_a_parts_content_type() {
        let rendered = body_client(
            "multipart/form-data",
            &json!({
                "schema": {"type": "object", "properties": {"metadata": {"type": "object"}}},
                "encoding": {"metadata": {"contentType": "application/json"}},
            }),
        )?;
        assert!(
            rendered.contains("content_type: ::std::option::Option::Some(\"application/json\")"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_content_type_listing_several_alternatives_is_not_an_instruction() {
        // `cloudflare` writes ten comma-separated types on one part. That is the set a server
        // accepts, not a decision about what to send, and sending the whole string as one header
        // would be sending a content type that is not one.
        let rendered = body_client(
            "multipart/form-data",
            &json!({
                "schema": {"type": "object", "properties": {"module": {"type": "string", "format": "binary"}}},
                "encoding": {"module": {"contentType": "text/javascript, application/wasm, text/plain"}},
            }),
        )?;
        assert!(!rendered.contains("application/wasm"), "{rendered}");
        assert!(
            rendered.contains("content_type: ::std::option::Option::None"),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn a_text_body_is_a_string_and_a_binary_one_is_bytes() {
        let text = body_client("text/plain", &json!({"schema": {"type": "string"}}))?;
        assert!(text.contains("::std::string::String"), "{text}");
        assert!(text.contains("\"text/plain\""), "{text}");

        let binary = body_client(
            "application/octet-stream",
            &json!({"schema": {"type": "string", "format": "binary"}}),
        )?;
        assert!(binary.contains("::std::vec::Vec<u8>"), "{binary}");
    }

    #[test_util::test]
    fn a_body_that_declares_only_a_wildcard_sends_something_a_server_can_read() {
        // `preference` sorts a wildcard last, which decides nothing when it is the only entry.
        // `Content-Type: */*` is not a content type, and 9 corpus bodies declare exactly that.
        let binary = body_client(
            "image/*",
            &json!({"schema": {"type": "string", "format": "binary"}}),
        )?;
        assert!(!binary.contains("\"image/*\""), "{binary}");
        assert!(binary.contains("application/octet-stream"), "{binary}");
        assert!(binary.contains("::std::vec::Vec<u8>"), "{binary}");
    }

    #[test_util::test]
    fn a_wildcard_over_a_real_schema_keeps_the_type_the_document_gave() {
        // `telnyx` writes `*/*` over a `$ref` to an object. A wildcard permits every content type,
        // so choosing bytes there would throw away a type the document supplied for no reason —
        // JSON is the one content type the declared schema actually fits.
        let rendered = body_client(
            "*/*",
            &json!({"schema": {"type": "object", "properties": {"start": {"type": "string"}}}}),
        )?;
        assert!(rendered.contains("request.json(body)"), "{rendered}");
        assert!(!rendered.contains("Vec<u8>"), "{rendered}");
    }

    #[test_util::test]
    fn a_default_that_claims_success_is_still_a_failure_variant() {
        // `weather-gov` writes a `302` and a `default` and no `2XX`, so the `default` is both the
        // success arm and the catch-all failure. Two answers about that used to disagree: the error
        // type counted two failure arms and declared an enum, while `send` counted one and decoded
        // the body straight into it — asking an enum that derives only `Debug, Clone` to
        // deserialize. The client did not compile, and no document in the quick tier had the shape.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/briefing": {"get": {
                "operationId": "getBriefing",
                "responses": {
                    "302": {"description": "found"},
                    "default": {"description": "problem", "content": {"application/json": {"schema": {"type": "string"}}}},
                },
            }}},
        }))?;
        assert!(rendered.contains("pub enum GetBriefingError"), "{rendered}");
        // Every failure path puts the body in a variant rather than handing the enum to serde.
        assert!(
            rendered.contains("GetBriefingError::Status302"),
            "{rendered}"
        );
        assert!(rendered.contains("GetBriefingError::Default"), "{rendered}");
    }

    #[test_util::test]
    fn a_deprecated_operation_does_not_warn_in_the_consumers_crate() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/artists": {"get": {
                "operationId": "getArtists",
                "deprecated": true,
                "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(rendered.contains("#[deprecated]"), "{rendered}");
        // The request's `params` field names the deprecated params type, and the struct being
        // deprecated itself exempts nothing — the field carries its own expectation.
        assert!(
            rendered.contains(")]\n    params: GetArtistsParams,"),
            "{rendered}"
        );
        // The request is deprecated, so its own `impl` block uses a deprecated type — a warning
        // about code the consumer did not write. An `impl` cannot be deprecated, so it says the
        // only thing that distinguishes "this is the deprecated thing" from "this uses one".
        assert!(
            rendered
                .contains("reason = \"the generated request deliberately exposes deprecated API\""),
            "{rendered}"
        );

        // The accessor is the same problem one level up: it is `#[deprecated]` itself, and it both
        // returns the deprecated request and constructs one. Carrying the attribute exempts it from
        // nothing — rustc lints the signature and the body — so it needs the expectation too.
        assert!(
            rendered.contains(
                "reason = \"the generated accessor deliberately returns a deprecated operation\""
            ),
            "{rendered}"
        );

        // And an operation the document does not deprecate carries neither.
        let plain = client(json!({
            "openapi": "3.1.0",
            "paths": {"/artists": {"get": {"operationId": "getArtists", "responses": {"200": {"description": "ok"}}}}},
        }))?;
        assert!(!plain.contains("deprecated"), "{plain}");
    }

    #[test_util::test]
    fn a_type_that_names_a_deprecated_type_does_not_warn_in_the_consumers_crate() {
        // `okta` deprecates the `MtlsTrustCredentialsRevocation` component and not the `revocation`
        // property that holds one. Both renderings are faithful — and rustc lints the use anyway,
        // so the generated crate warned on every build about code its consumer did not write.
        let rendered = types(
            with_schemas(json!({
                "Retired": {"type": "string", "enum": ["a", "b"], "deprecated": true},
                "Holder": {
                    "type": "object",
                    "properties": {"revocation": {"$ref": "#/components/schemas/Retired"}},
                },
            })),
            &Config::default(),
        )?;
        // The public type stays deprecated, while generated code names a transparent hidden alias.
        // A field-level expectation cannot reach the `Deserialize` derive expansion at the same
        // span, and an item-wide expectation would be too broad.
        assert!(
            rendered.contains(
                "reason = \"generated internals need a non-deprecated path to this public contract\""
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("pub(crate) mod __progeny_deprecated"),
            "{rendered}"
        );
        assert!(rendered.contains("pub(crate) type Retired"), "{rendered}");
        assert!(
            rendered.contains("pub revocation: Option<__progeny_deprecated::Retired>"),
            "{rendered}"
        );

        // A holder of something the document did not deprecate carries nothing.
        let plain = types(
            with_schemas(json!({
                "Live": {"type": "string", "enum": ["a", "b"]},
                "Holder": {
                    "type": "object",
                    "properties": {"revocation": {"$ref": "#/components/schemas/Live"}},
                },
            })),
            &Config::default(),
        )?;
        assert!(!plain.contains("allow(deprecated)"), "{plain}");
    }

    #[test_util::test]
    fn an_unreferenced_deprecated_type_has_no_unused_internal_alias() {
        let rendered = types(
            with_schemas(json!({
                "Retired": {"deprecated": true},
            })),
            &Config::default(),
        )?;
        assert!(rendered.contains("pub type Retired"), "{rendered}");
        assert!(!rendered.contains("pub(crate) type Retired"), "{rendered}");
    }

    #[test_util::test]
    fn an_operation_whose_only_response_is_default_still_has_a_success_path() {
        // `default` means "anything not otherwise claimed". With no `2XX` beside it, a `200` is
        // exactly that — so treating the arm as a failure would report every successful call as
        // one, which is what the first implementation did.
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {"default": {
                    "description": "whatever happens",
                    "content": {"application/json": {"schema": {"type": "string"}}},
                }},
            }}},
        }))?;
        assert!(
            rendered.contains("ResponseValue<String>, Error<String>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("200..=299 => ::std::result::Result::Ok"),
            "{rendered}"
        );

        // With a declared success beside it, `default` goes back to being the failure it claims.
        let with_success = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "default": {"description": "anything else", "content": {"application/json": {"schema": {"type": "string"}}}},
                },
            }}},
        }))?;
        assert!(
            !with_success.contains("200..=299 => ::std::result::Result::Ok"),
            "{with_success}"
        );
        assert!(
            with_success.contains("ResponseValue<()>, Error<String>"),
            "{with_success}"
        );
    }

    #[test_util::test]
    fn only_a_server_url_a_client_could_actually_use_becomes_a_default() {
        let with_server = |servers: Value| -> eyre::Result<String> {
            let mut document = json!({
                "openapi": "3.1.0",
                "paths": {"/pets": {"get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}}}},
            });
            document
                .as_object_mut()
                .ok_or_eyre("test fixture should be an object")?
                .insert("servers".to_owned(), servers);
            client(document)
        };

        let absolute = with_server(json!([{"url": "https://example.invalid/v1"}]))?;
        assert!(
            absolute.contains("impl ::std::default::Default"),
            "{absolute}"
        );
        assert!(
            absolute.contains("https://example.invalid/v1"),
            "{absolute}"
        );

        // A relative URL is what the description is served next to, and only the caller knows what
        // that was; a templated one has variables progeny does not substitute. Both are perfectly
        // good things for a document to say and neither is a default a client could send to.
        for servers in [
            json!([{"url": "/api/v3"}]),
            json!([{"url": "https://{region}.example.invalid"}]),
        ] {
            let rendered = with_server(servers)?;
            assert!(
                !rendered.contains("impl ::std::default::Default"),
                "{rendered}"
            );
        }
    }

    #[test_util::test]
    fn an_undeclared_status_is_handed_back_raw_rather_than_given_a_shape() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains("Error::UnexpectedStatus(response)"),
            "{rendered}"
        );

        // …unless the document declares a `default` arm, which is exactly what claims the rest.
        let with_default = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "responses": {
                    "200": {"description": "ok"},
                    "default": {"description": "anything else"},
                },
            }}},
        }))?;
        assert!(
            !with_default.contains("Error::UnexpectedStatus(response)"),
            "{with_default}"
        );
    }

    #[test_util::test]
    fn a_query_parameter_carries_the_style_the_document_asked_for() {
        let rendered = client(json!({
            "openapi": "3.1.0",
            "paths": {"/pets": {"get": {
                "operationId": "listPets",
                "parameters": [
                    {"name": "tags", "in": "query", "style": "pipeDelimited", "explode": false,
                     "schema": {"type": "array", "items": {"type": "string"}}},
                    {"name": "x-trace", "in": "header", "schema": {"type": "string"}},
                ],
                "responses": {"200": {"description": "ok"}},
            }}},
        }))?;
        assert!(
            rendered.contains("support::style::Style::PipeDelimited"),
            "{rendered}"
        );
        assert!(
            rendered.contains("support::style::query_pairs("),
            "{rendered}"
        );
        assert!(rendered.contains(r#""tags""#), "{rendered}");
        assert!(
            rendered.contains(r#".header("x-trace", support::style::header_value"#),
            "{rendered}"
        );
    }

    #[test_util::test]
    fn generation_is_deterministic() {
        let document = with_schemas(json!({
            "A": {"type": "object", "properties": {"b": {"$ref": "#/components/schemas/B"}}},
            "B": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        }));
        let once = types(document.clone(), &Config::default())?;
        let twice = types(document, &Config::default())?;
        assert_eq!(once, twice);
    }

    /// Every `support::…` path a renderer spells resolves to an item the shipped module holds.
    ///
    /// The renderers write these paths as *strings into somebody else's crate*: progeny compiles
    /// the support items, but nothing in progeny's own build calls them by the emitted spelling,
    /// so renaming `support::style::cookie_pair` would build green here and break every
    /// generated crate with a cookie parameter. This resolves each spelled path against the
    /// emitted support module's actual items — the check the compiler cannot do across the
    /// generation boundary.
    #[test_util::test]
    fn every_support_path_the_renderers_spell_exists() {
        use std::collections::BTreeSet;

        // One document exercising every wire feature: path styles, query styles, header and
        // cookie parameters, all five body kinds, and a server half.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets/{id}": {"get": {
                    "operationId": "getPet",
                    "parameters": [
                        {"name": "id", "in": "path", "required": true, "style": "matrix",
                         "schema": {"type": "string"}},
                        {"name": "tags", "in": "query", "style": "pipeDelimited", "explode": false,
                         "schema": {"type": "array", "items": {"type": "string"}}},
                        {"name": "x-trace", "in": "header", "schema": {"type": "string"}},
                        {"name": "session", "in": "cookie", "schema": {"type": "string"}},
                    ],
                    "responses": {"200": {"description": "ok", "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Pet"}}}},
                                  "404": {"description": "gone"}},
                }},
                "/pets": {"post": {
                    "operationId": "createPet",
                    "requestBody": {"content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Pet"}}}},
                    "responses": {"201": {"description": "made", "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Pet"}}}}},
                }},
                "/form": {"post": {
                    "operationId": "sendForm",
                    "requestBody": {"content": {"application/x-www-form-urlencoded": {
                        "schema": {"type": "object", "properties": {"a": {"type": "string"}}},
                        "encoding": {"a": {"style": "form", "explode": false}}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/upload": {"post": {
                    "operationId": "upload",
                    "requestBody": {"content": {"multipart/form-data": {
                        "schema": {"type": "object", "properties": {
                            "file": {"type": "string", "format": "binary"},
                            "note": {"type": "string"}}}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/note": {"post": {
                    "operationId": "sendNote",
                    "requestBody": {"content": {"text/plain": {"schema": {"type": "string"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/blob": {"post": {
                    "operationId": "sendBlob",
                    "requestBody": {"content": {"application/octet-stream": {
                        "schema": {"type": "string", "format": "binary"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
            "components": {"schemas": {
                "Pet": {"type": "object", "required": ["name"],
                        "properties": {"name": {"type": "string"}}},
            }},
        });
        let rendered = files(document, &Config::default())?;

        // Every item the emitted support module defines, as `module::item` relative paths —
        // plus the root-level spellings its glob re-exports (`pub use wire::*;`) create.
        let support = syn::parse_file(&rendered[camino::Utf8Path::new("src/support.rs")])?;
        let mut defined: BTreeSet<String> = BTreeSet::new();
        let mut reexported: Vec<String> = Vec::new();
        support_items(&support.items, "", &mut defined, &mut reexported);
        for module in reexported {
            let inner: Vec<String> = defined
                .iter()
                .filter_map(|path| path.strip_prefix(&format!("{module}::")))
                .map(ToOwned::to_owned)
                .collect();
            defined.extend(inner);
        }

        // Every `support::…` (or `super::support::…`) path the other modules spell.
        let mut spelled: BTreeSet<Vec<String>> = BTreeSet::new();
        for name in ["src/client.rs", "src/server.rs", "src/types.rs"] {
            let Some(text) = rendered.get(camino::Utf8Path::new(name)) else {
                continue;
            };
            let file = syn::parse_file(text)?;
            let mut visitor = SupportPaths {
                out: BTreeSet::new(),
            };
            syn::visit::Visit::visit_file(&mut visitor, &file);
            spelled.extend(visitor.out);
        }
        assert!(
            spelled.len() >= 10,
            "the fixture should exercise the wire: {spelled:?}"
        );

        // A spelled path resolves when some prefix of it names a defined item — the tail past
        // that is an enum variant or an associated function, which belongs to the item.
        for path in &spelled {
            let resolves =
                (1..=path.len()).any(|length| defined.contains(&path[..length].join("::")));
            assert!(
                resolves,
                "the renderers spell `support::{}`, which the shipped module does not define",
                path.join("::")
            );
        }
    }

    /// Every named item of the emitted support module, recursively, with `pub use x::*;`
    /// re-export modules collected for the caller to flatten.
    fn support_items(
        items: &[syn::Item],
        prefix: &str,
        defined: &mut std::collections::BTreeSet<String>,
        globs: &mut Vec<String>,
    ) {
        for item in items {
            let name = match item {
                syn::Item::Fn(it) => Some(it.sig.ident.to_string()),
                syn::Item::Struct(it) => Some(it.ident.to_string()),
                syn::Item::Enum(it) => Some(it.ident.to_string()),
                syn::Item::Trait(it) => Some(it.ident.to_string()),
                syn::Item::Type(it) => Some(it.ident.to_string()),
                syn::Item::Const(it) => Some(it.ident.to_string()),
                syn::Item::Mod(it) => {
                    let module = it.ident.to_string();
                    let inner = format!("{prefix}{module}::");
                    if let Some((_, items)) = &it.content {
                        support_items(items, &inner, defined, globs);
                    }
                    Some(module)
                }
                syn::Item::Use(it) => {
                    // `pub use wire::*;` at the root: remember the module whose items become
                    // root-level names.
                    if prefix.is_empty()
                        && let syn::UseTree::Path(path) = &it.tree
                        && matches!(*path.tree, syn::UseTree::Glob(_))
                    {
                        globs.push(path.ident.to_string());
                    }
                    None
                }
                _ => None,
            };
            if let Some(name) = name {
                defined.insert(format!("{prefix}{name}"));
            }
        }
    }

    /// Collects every path rooted at `support` (or `super::support`), without the root.
    struct SupportPaths {
        out: std::collections::BTreeSet<Vec<String>>,
    }

    impl<'ast> syn::visit::Visit<'ast> for SupportPaths {
        fn visit_path(&mut self, node: &'ast syn::Path) {
            let segments: Vec<String> = node
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let rest = match segments.first().map(String::as_str) {
                Some("support") => &segments[1..],
                Some("super") if segments.get(1).map(String::as_str) == Some("support") => {
                    &segments[2..]
                }
                _ => {
                    syn::visit::visit_path(self, node);
                    return;
                }
            };
            if !rest.is_empty() {
                self.out.insert(rest.to_vec());
            }
            syn::visit::visit_path(self, node);
        }
    }

    /// The API layer's style table and the shipped one are one variant list, held by the
    /// compiler.
    ///
    /// `style_tokens` spells the shipped enum's variants as *strings* into generated source, so
    /// no compiler links the two enums there. These two matches are that link: a variant added
    /// to either enum fails this test's build, where the drift it prevents — a style silently
    /// encoded as `form` by the shipped table's fallback, or generated crates that name a
    /// variant the shipped enum lacks — fails nothing in progeny's own build.
    #[test_util::test]
    fn the_two_style_enums_are_one_variant_list() {
        use crate::api::Style as Declared;
        use crate::support::style::Style as Shipped;
        fn bridge(style: Declared) -> Shipped {
            match style {
                Declared::Form => Shipped::Form,
                Declared::Simple => Shipped::Simple,
                Declared::Label => Shipped::Label,
                Declared::Matrix => Shipped::Matrix,
                Declared::SpaceDelimited => Shipped::SpaceDelimited,
                Declared::PipeDelimited => Shipped::PipeDelimited,
                Declared::DeepObject => Shipped::DeepObject,
            }
        }
        fn back(style: Shipped) -> Declared {
            match style {
                Shipped::Form => Declared::Form,
                Shipped::Simple => Declared::Simple,
                Shipped::Label => Declared::Label,
                Shipped::Matrix => Declared::Matrix,
                Shipped::SpaceDelimited => Declared::SpaceDelimited,
                Shipped::PipeDelimited => Declared::PipeDelimited,
                Shipped::DeepObject => Declared::DeepObject,
            }
        }
        for style in [
            Declared::Form,
            Declared::Simple,
            Declared::Label,
            Declared::Matrix,
            Declared::SpaceDelimited,
            Declared::PipeDelimited,
            Declared::DeepObject,
        ] {
            assert_eq!(back(bridge(style)), style);
        }
    }

    /// The manifest declares every crate the rendered surface names, not only what the named
    /// types use.
    ///
    /// A `format: uuid` that appears only as a parameter gets no named contract on purpose — a
    /// `pub type` alias for one scalar would be noise — but the params struct still spells `uuid::Uuid`.
    /// Binary request and response bodies under `formats.bytes = "bytes"` spell `::bytes::Bytes`
    /// outside the type layer. A manifest computed from the named contracts alone shipped these
    /// API-surface types as crates that do not compile.
    #[test_util::test]
    fn the_manifest_declares_what_the_surface_names_beyond_the_types() {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets/{id}": {"get": {
                    "operationId": "getPet",
                    "parameters": [{"name": "id", "in": "path", "required": true,
                        "schema": {"type": "string", "format": "uuid"}}],
                    "responses": {"200": {"description": "ok"}},
                }},
                "/upload": {"post": {
                    "operationId": "upload",
                    "requestBody": {"content": {"application/octet-stream": {
                        "schema": {"type": "string", "format": "binary"}}}},
                    "responses": {"204": {"description": "done"}},
                }},
                "/download": {"get": {
                    "operationId": "download",
                    "responses": {"200": {"description": "bytes", "content": {
                        "application/octet-stream": {
                            "schema": {
                                "type": "string",
                                "contentMediaType": "application/octet-stream"
                            }
                        }
                    }}},
                }},
            },
        });
        let config = Config {
            formats: crate::config::Formats {
                uuid: crate::config::UuidCrate::Uuid,
                bytes: crate::config::BytesRepr::Bytes,
                ..crate::config::Formats::default()
            },
            ..Config::default()
        };
        let rendered = files(document, &config)?;
        let manifest = &rendered[camino::Utf8Path::new("Cargo.toml")];
        assert!(manifest.contains("uuid = { version = \"1\""), "{manifest}");
        assert!(manifest.contains("dep:bytes"), "{manifest}");
        assert!(manifest.contains("bytes = { version = \"1\""), "{manifest}");
        // The claim about where the crate is named, pinned so the manifest rule can follow it.
        let client = &rendered[camino::Utf8Path::new("src/client.rs")];
        assert!(client.contains("uuid::Uuid"), "{client}");
        assert!(client.contains("::bytes::Bytes"), "{client}");
        let server = &rendered[camino::Utf8Path::new("src/server.rs")];
        assert!(server.contains("Box<::bytes::Bytes>"), "{server}");
        assert!(
            server.contains(r#"respond_bytes(200u16, "application/octet-stream""#),
            "{server}"
        );
        let types = &rendered[camino::Utf8Path::new("src/types.rs")];
        assert!(!types.contains("uuid::Uuid"), "{types}");
    }

    #[test_util::test]
    fn a_server_only_crate_under_the_bytes_knob_ships_a_manifest_cargo_accepts() {
        // The `server` feature names `dep:bytes` for a raw body or response, and the
        // dependency line lived inside the client-only block: a server-only crate shipped a
        // manifest cargo refuses, with generation reporting success. The raw *request* body is
        // the other half: the handler argument says `::bytes::Bytes`, so the extraction has to
        // convert what the reader hands back.
        let document = json!({
            "openapi": "3.1.0",
            "paths": {
                "/upload": {"post": {
                    "operationId": "upload",
                    "requestBody": {"content": {"application/octet-stream": {
                        "schema": {"type": "string", "contentMediaType": "application/octet-stream"},
                    }}},
                    "responses": {"204": {"description": "done"}},
                }},
            },
        });
        let config = Config {
            emit: crate::config::Emit {
                types: true,
                client: false,
                server: true,
            },
            formats: crate::config::Formats {
                bytes: crate::config::BytesRepr::Bytes,
                ..crate::config::Formats::default()
            },
            ..Config::default()
        };
        let rendered = files(document, &config)?;
        let manifest = &rendered[camino::Utf8Path::new("Cargo.toml")];
        assert!(manifest.contains("dep:bytes"), "{manifest}");
        assert!(manifest.contains("bytes = { version = \"1\""), "{manifest}");
        let server = &rendered[camino::Utf8Path::new("src/server.rs")];
        assert!(server.contains("map(::bytes::Bytes::from)"), "{server}");
    }

    #[test_util::test]
    fn a_base64_property_stays_wire_text_under_the_bytes_representation_knob() {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {},
            "components": {"schemas": {
                "Envelope": {
                    "type": "object",
                    "required": ["payload"],
                    "properties": {
                        "payload": {"type": "string", "contentEncoding": "base64"},
                    },
                },
            }},
        });
        let config = Config {
            formats: crate::config::Formats {
                bytes: crate::config::BytesRepr::Bytes,
                ..crate::config::Formats::default()
            },
            ..Config::default()
        };

        let rendered = files(document, &config)?;
        let types = &rendered[camino::Utf8Path::new("src/types.rs")];
        assert!(types.contains("pub payload: String"), "{types}");
        assert!(!types.contains("pub payload: ::bytes::Bytes"), "{types}");
    }

    #[test_util::test]
    fn ip_formats_use_the_dependency_free_standard_library_types() {
        let document = json!({
            "openapi": "3.1.0",
            "paths": {},
            "components": {"schemas": {
                "Address": {"type": "string", "format": "ip"},
                "V4Address": {"type": "string", "format": "ipv4"},
                "V6Address": {"type": "string", "format": "ipv6"},
            }},
        });

        let rendered = files(document, &Config::default())?;
        let types = &rendered[camino::Utf8Path::new("src/types.rs")];
        assert!(types.contains("::std::net::IpAddr"), "{types}");
        assert!(types.contains("::std::net::Ipv4Addr"), "{types}");
        assert!(types.contains("::std::net::Ipv6Addr"), "{types}");
    }
}
