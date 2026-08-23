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
//! * the declared response **decodes back** in the client, with the declared status.
//!
//! Skipped operations are counted and named — a probe that silently narrows its corpus reads as
//! "covered" when it is not.

use std::fmt::Write as _;

use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use progeny::harness::{Probe, ProbeOp};

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

        let plan = progeny::harness::probe(bytes, &config)
            .wrap_err_with(|| format!("planning the probe for {name}"))?;
        let output =
            progeny::generate(bytes, &config).wrap_err_with(|| format!("generating {name}"))?;
        let directory = crate::generated::write(&format!("probe-{name}"), &output)?;

        crate::generated::write_wire_test(
            &directory,
            "probe.rs",
            &render(&config.package.name, &plan),
        )?;

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
            .args(["test", "--quiet", "--all-features", "--test", "probe"])
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

/// The probe test file: the double implementing every servable operation, then one driver test per
/// driven one.
fn render(krate: &str, plan: &Probe) -> String {
    let krate = crate::corpus::lib_name(krate);
    let uses_types = plan.operations.iter().any(|operation| {
        operation
            .body
            .as_ref()
            .is_some_and(|body| body.ty.contains("types::"))
            || operation
                .inputs
                .iter()
                .any(|input| input.json.is_some() && input.ty.contains("types::"))
    });
    let types_import = if uses_types {
        format!("use {krate}::types;")
    } else {
        String::new()
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        indoc::formatdoc! {r#"
            //! Generated by `cargo xtask probe`. Every servable operation of this description,
            //! sent by the generated client to the generated server over a socket.

            use color_eyre::eyre;
            use {krate}::{{client, server}};
            {types_import}

            /// Read a synthesized value out of its JSON spelling.
            fn value<T: serde::de::DeserializeOwned>(json: &str) -> T {{
                #[expect(
                    clippy::expect_used,
                    reason = "the probe serializes this value from the same generated contract"
                )]
                serde_json::from_str(json)
                    .expect("a synthesized value matches its own contract")
            }}

            #[derive(Debug, Clone)]
            struct Double;

            async fn serving() -> eyre::Result<client::Client> {{
                let router = server::router(Double);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await?;
                let address = listener.local_addr()?;
                tokio::spawn(async move {{
                    let _ = axum::serve(listener, router).await;
                }});
                Ok(client::Client::new(format!("http://{{address}}")))
            }}

            impl server::Api for Double {{"#
        }
    );
    for operation in &plan.operations {
        out.push_str(&handler(operation));
    }
    out.push_str("}\n");
    for operation in &plan.operations {
        if operation.skip.is_none()
            && let Some(answer) = &operation.response.answer
        {
            out.push_str(&driver(operation, answer));
        }
    }
    out
}

/// One trait method on the double: presence assertions, then the synthesized response.
fn handler(operation: &ProbeOp) -> String {
    let mut out = String::new();
    let arguments = handler_arguments(operation);
    out.push('\n');
    if operation.body.as_ref().is_some_and(|body| body.deprecated) {
        let _ = writeln!(
            out,
            "    #[expect(deprecated, reason = \"the probe deliberately implements an operation \
             whose body type is deprecated\")]"
        );
    }
    let _ = writeln!(
        out,
        "    async fn {}(&self{arguments}) -> server::{} {{",
        operation.method, operation.response.enum_name
    );
    if operation.skip.is_some() {
        let _ = writeln!(out, "        unimplemented!(\"skipped by the probe plan\")");
        out.push_str("    }\n");
        return out;
    }
    // A plan with no skip carries an answer; the type makes reading a sentinel impossible, and
    // this keeps an answerless method honest if that invariant ever moves.
    let Some(answer) = &operation.response.answer else {
        let _ = writeln!(
            out,
            "        unimplemented!(\"the plan carries no answer\")"
        );
        out.push_str("    }\n");
        return out;
    };
    write_arrival_assertions(&mut out, operation);
    let value = answer_value(answer);
    let payload = if answer.boxed {
        format!("Box::new({value})")
    } else {
        value
    };
    let _ = writeln!(
        out,
        "        server::{}::{}({payload})",
        operation.response.enum_name, answer.variant,
    );
    out.push_str("    }\n");
    out
}

fn handler_arguments(operation: &ProbeOp) -> String {
    let mut arguments = String::new();
    for group in &operation.groups {
        let arg = if operation.skip.is_some() || group.optional_fields.is_empty() {
            format!("_{}", group.arg)
        } else {
            group.arg.to_owned()
        };
        let _ = write!(arguments, ", {arg}: server::{}", group.ty);
    }
    if let Some(body) = &operation.body {
        // An optional body reaches the trait as an `Option`, exactly like an optional parameter
        // reaches its group struct.
        let arg = if operation.skip.is_some() || body.required {
            "_body"
        } else {
            "body"
        };
        if body.required {
            let _ = write!(arguments, ", {arg}: {}", body.ty);
        } else {
            let _ = write!(arguments, ", {arg}: ::std::option::Option<{}>", body.ty);
        }
    }
    arguments
}

fn write_arrival_assertions(out: &mut String, operation: &ProbeOp) {
    for group in &operation.groups {
        for field in &group.optional_fields {
            if field.deprecated {
                let arrived = format!("{}_{}_arrived", group.arg, field.name);
                let _ = write!(
                    out,
                    "{}",
                    indoc::formatdoc! {r#"
                            {indent}#[expect(
                            {indent}    deprecated,
                            {indent}    reason = "the probe deliberately reads this deprecated parameter"
                            {indent})]
                            {indent}let {arrived} = {arg}.{field}.is_some();
                            {indent}assert!(
                            {indent}    {arrived},
                            {indent}    "`{field}` was set by the driver and dropped on the way to the handler"
                            {indent});
                        "#,
                        indent = "        ",
                        arg = group.arg,
                        field = field.name,
                    }
                );
            } else {
                let _ = writeln!(
                    out,
                    "        assert!({}.{}.is_some(), \"`{}` was set by the driver and \
                     dropped on the way to the handler\");",
                    group.arg, field.name, field.name
                );
            }
        }
    }
    if operation.body.as_ref().is_some_and(|body| !body.required) {
        let _ = writeln!(
            out,
            "        assert!(body.is_some(), \"the body was set by the driver and dropped on \
             the way to the handler\");"
        );
    }
}

fn answer_value(answer: &progeny::harness::ProbeAnswer) -> String {
    match &answer.value {
        progeny::harness::ProbeValue::Json { json } => {
            format!("value({})", json_literal(json))
        }
        progeny::harness::ProbeValue::Text(text) => {
            format!("{}.to_owned()", json_literal(text))
        }
        progeny::harness::ProbeValue::Bytes { ty, bytes } => {
            if ty == "::bytes::Bytes" {
                format!("::bytes::Bytes::from_static(&{:?})", bytes.as_slice())
            } else {
                format!("::std::vec!{:?}", bytes.as_slice())
            }
        }
        progeny::harness::ProbeValue::Empty => "()".to_owned(),
    }
}

/// One driver test: build the params literal, send, and expect the declared status back.
fn driver(operation: &ProbeOp, answer: &progeny::harness::ProbeAnswer) -> String {
    let mut out = String::new();
    out.push('\n');
    let _ = writeln!(out, "#[test_util::test]");
    let _ = writeln!(out, "async fn probe_{}() {{", operation.method);
    let _ = writeln!(out, "    let client = serving().await?;");
    if operation.deprecated
        || operation.inputs.iter().any(|input| input.deprecated)
        || operation.body.as_ref().is_some_and(|body| body.deprecated)
    {
        let _ = writeln!(
            out,
            "    #[expect(deprecated, reason = \"the probe deliberately calls this deprecated \
             generated API\")]"
        );
    }
    if let Some(params) = &operation.params {
        let _ = writeln!(
            out,
            "    let response = client.{}({params} {{",
            operation.method
        );
        for input in &operation.inputs {
            let value = match &input.json {
                // An optional parameter whose name the wire erases: the literal must still name
                // the field, and `None` is the only value the server could ever tell apart.
                None => "None".to_owned(),
                Some(json) if input.required => {
                    format!("value::<{}>({})", input.ty, json_literal(json))
                }
                Some(json) => {
                    format!("Some(value::<{}>({}))", input.ty, json_literal(json))
                }
            };
            let _ = writeln!(out, "        {}: {value},", input.field);
        }
        if let Some(body) = &operation.body {
            let value = format!("value::<{}>({})", body.ty, json_literal(&body.json));
            if body.required {
                let _ = writeln!(out, "        body: {value},");
            } else {
                let _ = writeln!(out, "        body: Some({value}),");
            }
        }
        let _ = writeln!(out, "    }})");
    } else {
        let _ = writeln!(out, "    let response = client.{}()", operation.method);
    }
    let _ = writeln!(out, "        .send()");
    let _ = writeln!(out, "        .await");
    let _ = writeln!(out, "        ?;");
    let _ = writeln!(
        out,
        "    assert_eq!(response.status().as_u16(), {});",
        answer.status
    );
    let _ = writeln!(out, "}}");
    out
}

/// A JSON value as a Rust string literal, whatever it contains.
fn json_literal(json: &str) -> String {
    format!("{json:?}")
}
