//! The wire differential: every operation of a document, sent by its generated client to its
//! generated server.
//!
//! `xtask example` is this exact idea for one hand-written document; the probe is the generated
//! form of it for any document. The plan — the params literal to build, what the double answers,
//! which operations cannot be driven and why — comes from `progeny::harness::probe`, built from the same
//! frozen contracts the renderers read. What is asserted per operation:
//!
//! * the request the client builds **extracts cleanly** in the server — no rejection, which is the
//!   failure mode both of the example crate's first-minute catches surfaced as;
//! * every optional parameter the driver set **arrives `Some`** in the handler, which catches a
//!   parameter dropped without a rejection;
//! * the declared response **decodes back** in the client, with the declared status;
//! * every typed header the double answers with **decodes back equal** in the client, which is
//!   the one value the probe compares, because a header read in another spelling than it was
//!   written in is not refused but silently arrives as something else;
//! * the `(method, MatchedPath)` a layer on the generated router recorded **resolves through the
//!   reflection** to the route that was driven, `operations::Route::from_matched` — including one
//!   `HEAD` request axum dispatched to a `GET` handler, which the reflection has to name as the
//!   `GET` route.
//!
//! Skipped operations are counted and named — a probe that silently narrows its corpus reads as
//! "covered" when it is not.

use camino::Utf8Path;
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use proc_macro2::{Ident, Literal, TokenStream};
use progeny::harness::{Probe, ProbeOp};
use quote::{format_ident, quote};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Corpus documents to probe. Defaults to the quick tier.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Write the probe crates and stop, without compiling or running anything.
    #[arg(long)]
    generate_only: bool,

    /// Which serde strategy to generate with, rather than the configuration default.
    ///
    /// A serde-path defect that shows up only over a socket — a rejection rather than a payload
    /// mismatch — is exactly this gate's class, and until this flag the request-sending gates
    /// only ever drove the default strategy.
    #[arg(long, value_name = "STRATEGY")]
    serde: Option<crate::corpus::SerdeChoice>,
}

pub fn run(args: &Args) -> eyre::Result<()> {
    crate::generated::require_cargo()?;

    let wanted: Vec<String> = if args.specs.is_empty() {
        crate::corpus::quick_tier()?
    } else {
        args.specs.clone()
    };
    let documents = crate::corpus::selected(&wanted)?;

    let mut failures = 0usize;
    for (spec, bytes) in &documents {
        let name = &spec.name;
        let mut config = crate::corpus::config_for(spec);
        if let Some(choice) = args.serde {
            config.serde_impl = choice.into();
        }
        // The types, the client and the server as crates of their own, each compiled by a rustc
        // of its own: as one crate, cloudflare's library alone needs 13.7 GiB, which a 16 GiB
        // hosted runner does not survive.
        config.packaging = progeny::Packaging::Workspace;

        let plan = progeny::harness::probe(bytes, &config)
            .wrap_err_with(|| format!("planning the probe for {name}"))?;
        let output =
            progeny::generate(bytes, &config).wrap_err_with(|| format!("generating {name}"))?;
        let directory = crate::generated::write(&format!("probe-{name}"), &output)?;

        add_probe_package(&directory, &config.package.name)?;
        let source = crate::generated::rust_source(
            render(&config.package.name, &plan),
            "tests/probe.rs",
            name,
        )?;
        crate::generated::write_wire_test(&directory, "probe.rs", &source)?;

        let driven = plan
            .operations
            .iter()
            .filter(|operation| operation.skip.is_none())
            .count();
        let skipped: Vec<&ProbeOp> = plan
            .operations
            .iter()
            .filter(|operation| operation.skip.is_some())
            .collect();
        println!(
            "probe: {name}: {driven} operations driven, {} skipped",
            skipped.len()
        );
        for operation in &skipped {
            println!(
                "         skip {}: {}",
                operation.method,
                operation.skip.as_deref().unwrap_or_default()
            );
        }
        let head = match &plan.head {
            Some(head) => format!(
                "HEAD {} is served by the GET route `{}`",
                head.path, head.route
            ),
            None => "no driven parameterless GET, so the HEAD fallback is not driven".to_owned(),
        };
        println!("         {head}");
        if args.generate_only {
            println!("         written but not run: {directory}/tests/probe.rs");
            continue;
        }

        let mut command = crate::generated::cargo(&directory);
        // Cloudflare's synthesized access-application value is deep enough to exhaust libtest's
        // default thread stack while serde walks it. This changes only the generated test threads;
        // 16 MiB gives that standing corpus case enough headroom.
        let run = command
            .env("RUST_MIN_STACK", "16777216")
            // `--workspace`, so that `--all-features` reaches the generated crates as well.
            .args([
                "test",
                "--quiet",
                "--workspace",
                "--all-features",
                "--test",
                "probe",
            ])
            .output()
            .wrap_err("running the probe")?;
        if !run.status.success() {
            failures += 1;
            println!(
                "{}",
                indoc::formatdoc! {"
                    probe: {name} FAILED
                    {}{}",
                    String::from_utf8_lossy(&run.stdout),
                    String::from_utf8_lossy(&run.stderr)
                }
            );
        }
        // A probe binary is built in debug with full debug information, the largest at about 2 GB.
        crate::generated::clean(&directory, 0, None)?;
    }

    if failures > 0 {
        bail!("{failures} documents have a client and server that disagree on the wire");
    }
    println!(
        "probe: every driven operation of {} documents extracts cleanly and answers with its \
         declared status",
        documents.len()
    );
    Ok(())
}

/// Make the generated workspace's root the package the probe test belongs to.
///
/// The test drives the client against the server, so it depends on both crates, and on the
/// libraries its double names itself.
/// It names the types crate `generated`, which is where the harness roots the body types the
/// double's signatures spell.
fn add_probe_package(directory: &Utf8Path, package: &str) -> eyre::Result<()> {
    let manifest = directory.join("Cargo.toml");
    let workspace =
        std::fs::read_to_string(&manifest).wrap_err_with(|| format!("reading {manifest}"))?;
    std::fs::write(
        &manifest,
        indoc::formatdoc! {r#"
            {workspace}
            [package]
            name = "{package}-probe"
            version = "0.0.0"
            edition = "2024"
            publish = false

            [dependencies]
            {package}-client = {{ path = "{package}-client" }}
            {package}-server = {{ path = "{package}-server" }}
            generated = {{ package = "{package}-types", path = "{package}-types" }}
            axum = "0.8"
            serde = "1.0.164"
            serde_json = "1"
        "#},
    )
    .wrap_err_with(|| format!("writing {manifest}"))
}

/// The probe test file: the double implementing every servable operation, then one driver test per
/// driven one.
fn render(package: &str, plan: &Probe) -> TokenStream {
    let client = format_ident!("{}", crate::corpus::lib_name(&format!("{package}-client")));
    let server = format_ident!("{}", crate::corpus::lib_name(&format!("{package}-server")));
    let compares_headers = plan.operations.iter().any(|operation| {
        operation.skip.is_none()
            && operation
                .response
                .answer
                .as_ref()
                .and_then(|answer| answer.headers.as_ref())
                .is_some_and(|headers| !headers.fields.is_empty())
    });
    let preamble = preamble(&client, &server, compares_headers);
    let handlers = plan.operations.iter().map(handler);
    let drivers = plan.operations.iter().filter_map(|operation| {
        let answer = operation.response.answer.as_ref()?;
        operation.skip.is_none().then(|| driver(operation, answer))
    });
    let head = plan.head.as_ref().map(head_driver);
    quote! {
        #preamble

        impl server::Api for Double {
            type Context = ();

            #(#handlers)*
        }

        #(#drivers)*
        #head
    }
}

/// The `HEAD` case: axum serves it with the `GET` handler and reports the `GET` template, and
/// the reflection has to say which route that was.
fn head_driver(head: &progeny::harness::ProbeHead) -> TokenStream {
    let route = &head.route;
    quote! {
        #[test_util::test]
        async fn probe_head_resolves_to_the_get_route() {
            let (client, matched) = serving().await?;
            let url = format!("{}{}", client.base_url(), operations::Route::#route.path());
            let _ = client.client().head(url).send().await?;
            let recorded = recorded(&matched);
            assert_eq!(
                recorded.last().map(|(method, _)| method.as_str()),
                Some("HEAD"),
                "{recorded:?}"
            );
            assert_eq!(operations::Route::#route.method(), operations::Method::Get);
            assert_matched(&matched, operations::Route::#route)?;
        }
    }
}

/// Everything above the double: imports, the value reader, the recording router and the
/// reflection cross-check.
fn preamble(client: &Ident, server: &Ident, compares_headers: bool) -> TokenStream {
    // Only a driver that compares a header names it, and an unused helper is a warning.
    let like = compares_headers.then(|| {
        quote! {
            /// A synthesized value read as the type of the field it is compared with.
            fn like<T: serde::de::DeserializeOwned>(_field: &T, json: &str) -> T {
                value(json)
            }
        }
    });
    quote! {
        //! Generated by `cargo xtask probe`. Every servable operation of this description,
        //! sent by the generated client to the generated server over a socket.

        use std::sync::{Arc, Mutex};

        use color_eyre::eyre::{self, OptionExt as _};
        use #client::{client, operations};
        use #server::server;

        /// Read a synthesized value out of its JSON spelling.
        fn value<T: serde::de::DeserializeOwned>(json: &str) -> T {
            #[expect(
                clippy::expect_used,
                reason = "the probe serializes this value from the same generated contract"
            )]
            serde_json::from_str(json)
                .expect("a synthesized value matches its own contract")
        }

        #like

        #[derive(Debug, Clone)]
        struct Double;

        /// What the router matched, per request: the request-line method and the template
        /// axum's `MatchedPath` reports, recorded by a layer behind the routing step.
        type Matched = Arc<Mutex<Vec<(String, String)>>>;

        fn recorded(matched: &Matched) -> Vec<(String, String)> {
            match matched.lock() {
                Ok(seen) => seen.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        async fn serving() -> eyre::Result<(client::Client, Matched)> {
            let matched: Matched = Matched::default();
            let recorder = matched.clone();
            let router = server::router(Double).layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let recorder = recorder.clone();
                    async move {
                        let method = request.method().as_str().to_owned();
                        let path = request
                            .extensions()
                            .get::<axum::extract::MatchedPath>()
                            .map(|matched| matched.as_str().to_owned());
                        if let Some(path) = path {
                            let mut seen = match recorder.lock() {
                                Ok(seen) => seen,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            seen.push((method, path));
                        }
                        next.run(request).await
                    }
                },
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await?;
            let address = listener.local_addr()?;
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Ok((client::Client::new(format!("http://{address}")), matched))
        }

        /// The one request this server saw resolves, through the reflection, to the route
        /// the driver meant to hit.
        fn assert_matched(matched: &Matched, expected: operations::Route) -> eyre::Result<()> {
            let recorded = recorded(matched);
            let (method, path) = recorded
                .last()
                .ok_or_eyre("the router matched nothing")?;
            let token = operations::Method::from_token(method)
                .ok_or_eyre(format!("`{method}` is not a method the reflection knows"))?;
            assert_eq!(
                operations::Route::from_matched(token, path),
                Some(expected),
                "`{method} {path}` was matched, and the reflection does not resolve it to `{expected:?}`"
            );
            Ok(())
        }
    }
}

/// One trait method on the double: presence assertions, then the synthesized response.
fn handler(operation: &ProbeOp) -> TokenStream {
    let method = &operation.method;
    let enum_name = &operation.response.enum_name;
    let arguments = handler_arguments(operation);
    let deprecated = operation
        .body
        .as_ref()
        .is_some_and(|body| body.deprecated)
        .then(|| {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the probe deliberately implements an operation whose body type is deprecated"
                )]
            }
        });
    let body = match (&operation.skip, &operation.response.answer) {
        (Some(_), _) => quote! { unimplemented!("skipped by the probe plan") },
        // A plan with no skip carries an answer; the type makes reading a sentinel impossible, and
        // this keeps an answerless method honest if that invariant ever moves.
        (None, None) => quote! { unimplemented!("the plan carries no answer") },
        (None, Some(answer)) => answer_body(operation, answer),
    };
    quote! {
        #deprecated
        async fn #method(&self, #(#arguments),*) -> server::#enum_name {
            #body
        }
    }
}

/// What a driven method does: assert every optional input arrived, then answer.
fn answer_body(operation: &ProbeOp, answer: &progeny::harness::ProbeAnswer) -> TokenStream {
    let arrivals = arrival_assertions(operation);
    // Every variant is a struct variant: the status a range answers with, the headers, which the
    // double fills where the arm types them, and the body where the arm declares content.
    let status = answer.ranged.then(|| {
        let status = Literal::u16_unsuffixed(answer.status);
        quote! { status: #status, }
    });
    let (header_group, headers) = match &answer.headers {
        Some(headers) => (Some(header_group(headers)), quote! { Box::new(headers) }),
        None => (None, quote! { ::std::default::Default::default() }),
    };
    let body = match &answer.value {
        progeny::harness::ProbeValue::Empty => None,
        _ if answer.boxed => {
            let value = answer_value(answer);
            Some(quote! { , body: Box::new(#value) })
        }
        _ => {
            let value = answer_value(answer);
            Some(quote! { , body: #value })
        }
    };
    let (enum_name, variant) = (&operation.response.enum_name, &answer.variant);
    quote! {
        #(#arrivals)*
        #header_group
        server::#enum_name::#variant { #status headers: #headers #body }
    }
}

/// The `headers` the double answers with: every typed header, and no raw ones.
fn header_group(headers: &progeny::harness::ProbeHeaders) -> TokenStream {
    let deprecated = headers
        .fields
        .iter()
        .any(|field| field.deprecated)
        .then(|| {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the probe deliberately answers with a header the description deprecates"
                )]
            }
        });
    let group = &headers.group;
    let fields = headers.fields.iter().map(|header| {
        let (field, json) = (&header.field, header.json.as_str());
        quote! { #field: Some(value(#json)), }
    });
    quote! {
        #deprecated
        let headers = server::#group {
            #(#fields)*
            ..::std::default::Default::default()
        };
    }
}

fn handler_arguments(operation: &ProbeOp) -> Vec<TokenStream> {
    // The double asks nothing of the request beyond what the description declares.
    let mut arguments = vec![quote! { _context: () }];
    for group in &operation.groups {
        let arg = if operation.skip.is_some() || group.optional_fields.is_empty() {
            format_ident!("_{}", group.arg)
        } else {
            group.arg.clone()
        };
        let ty = &group.ty;
        arguments.push(quote! { #arg: server::#ty });
    }
    if let Some(body) = &operation.body {
        // An optional body reaches the trait as an `Option`, exactly like an optional parameter
        // reaches its group struct.
        let arg = if operation.skip.is_some() || body.required {
            format_ident!("_body")
        } else {
            format_ident!("body")
        };
        let ty = &body.ty;
        if body.required {
            arguments.push(quote! { #arg: #ty });
        } else {
            arguments.push(quote! { #arg: ::std::option::Option<#ty> });
        }
    }
    arguments
}

fn arrival_assertions(operation: &ProbeOp) -> Vec<TokenStream> {
    let mut assertions = Vec::new();
    for group in &operation.groups {
        let arg = &group.arg;
        for field in &group.optional_fields {
            let name = &field.name;
            let message =
                format!("`{name}` was set by the driver and dropped on the way to the handler");
            if field.deprecated {
                let arrived = format_ident!("{}_{}_arrived", arg, name);
                assertions.push(quote! {
                    #[expect(
                        deprecated,
                        reason = "the probe deliberately reads this deprecated parameter"
                    )]
                    let #arrived = #arg.#name.is_some();
                    assert!(#arrived, #message);
                });
            } else {
                assertions.push(quote! { assert!(#arg.#name.is_some(), #message); });
            }
        }
    }
    if operation.body.as_ref().is_some_and(|body| !body.required) {
        assertions.push(quote! {
            assert!(
                body.is_some(),
                "the body was set by the driver and dropped on the way to the handler"
            );
        });
    }
    assertions
}

fn answer_value(answer: &progeny::harness::ProbeAnswer) -> TokenStream {
    match &answer.value {
        progeny::harness::ProbeValue::Json { json } => {
            let json = json.as_str();
            quote! { value(#json) }
        }
        progeny::harness::ProbeValue::Text(text) => {
            let text = text.as_str();
            quote! { #text.to_owned() }
        }
        progeny::harness::ProbeValue::Bytes(bytes) => {
            let bytes = bytes.iter().map(|byte| Literal::u8_unsuffixed(*byte));
            quote! { ::axum::body::Body::from(::std::vec![#(#bytes),*]) }
        }
        progeny::harness::ProbeValue::Empty => quote! { () },
    }
}

/// One driver test: build the params literal, send, and expect the declared status back.
///
/// The literal names no type: each value takes its type from the params struct's field.
fn driver(operation: &ProbeOp, answer: &progeny::harness::ProbeAnswer) -> TokenStream {
    let method = &operation.method;
    let test = format_ident!("probe_{}", method);
    let deprecated = operation.deprecated.then(|| {
        quote! {
            #[expect(
                deprecated,
                reason = "the probe deliberately calls this deprecated generated API"
            )]
        }
    });
    let call = if let Some(params) = &operation.params {
        let inputs = operation.inputs.iter().map(|input| {
            let field = &input.field;
            let value = match input.json.as_deref() {
                // An optional parameter whose name the wire erases: the literal must still name
                // the field, and `None` is the only value the server could ever tell apart.
                None => quote! { None },
                Some(json) if input.required => quote! { value(#json) },
                Some(json) => quote! { Some(value(#json)) },
            };
            quote! { #field: #value, }
        });
        let body = operation.body.as_ref().map(|body| {
            let json = body.json.as_str();
            if body.required {
                quote! { body: value(#json), }
            } else {
                quote! { body: Some(value(#json)), }
            }
        });
        quote! { client.#method(client::#params { #(#inputs)* #body }) }
    } else {
        quote! { client.#method() }
    };
    let status = Literal::u16_unsuffixed(answer.status);
    let route = &operation.route;
    let headers = answer
        .headers
        .as_ref()
        .map(|headers| header_assertions(headers, &answer.variant));
    quote! {
        #[test_util::test]
        async fn #test() {
            let (client, matched) = serving().await?;
            #deprecated
            let response = #call.send().await?;
            assert_eq!(response.status().as_u16(), #status);
            assert_matched(&matched, operations::Route::#route)?;
            #headers
        }
    }
}

/// The driver's half of the header differential: every header the double set reads back in the
/// client as the value it was set to.
///
/// Compared through JSON, because a generated type carries only the derives its configuration
/// asked for, and on the client field's own type, whichever decoding made it an `Option`.
fn header_assertions(headers: &progeny::harness::ProbeHeaders, variant: &Ident) -> TokenStream {
    let payload = if let Some(success) = &headers.success {
        quote! {
            let client::#success::#variant(payload) = response.value() else {
                eyre::bail!("the response decoded as another success arm");
            };
        }
    } else {
        quote! { let payload = response.value(); }
    };
    let fields = headers.fields.iter().map(|header| {
        let deprecated = header.deprecated.then(|| {
            quote! {
                #[expect(
                    deprecated,
                    reason = "the probe deliberately reads a header the description deprecates"
                )]
            }
        });
        let (field, json) = (&header.field, header.json.as_str());
        let message = format!(
            "the `{}` header the server wrote did not decode back to the same value",
            header.wire_name
        );
        quote! {
            {
                #deprecated
                let field = &payload.headers.#field;
                assert_eq!(
                    serde_json::to_value(field)?,
                    serde_json::to_value(like(field, #json))?,
                    #message,
                );
            }
        }
    });
    quote! {
        #payload
        #(#fields)*
    }
}
