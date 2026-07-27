//! The wire differential: every operation of a document, sent by its generated client to its
//! generated server.
//!
//! `xtask example` is this exact idea for one hand-written document; the probe is the generated
//! form of it for any document. The plan — which setters to call, what the double answers, which
//! operations cannot be driven and why — comes from `progeny::harness::probe`, built from the same
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
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use progeny::harness::{Probe, ProbeOp};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Corpus documents to probe. Defaults to the quick tier.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Write the probe crates and stop, without compiling or running anything.
    #[arg(long)]
    generate_only: bool,
}

pub fn run(args: &Args) -> Result<()> {
    crate::generated::require_cargo()?;

    let manifest = crate::corpus::load_manifest()?;
    let selected: Vec<String> = if args.specs.is_empty() {
        crate::corpus::quick_tier()?
    } else {
        args.specs.clone()
    };

    let mut failures = 0usize;
    for name in &selected {
        let spec = manifest
            .iter()
            .find(|spec| &spec.name == name)
            .with_context(|| format!("no corpus document named `{name}`"))?;
        let path = crate::corpus::document_path(spec);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
        let config = crate::corpus::config_for(spec);

        let plan = progeny::harness::probe(&bytes, &config)
            .with_context(|| format!("planning the probe for {name}"))?;
        let output =
            progeny::generate(&bytes, &config).with_context(|| format!("generating {name}"))?;
        let directory = crate::generated::write(&format!("probe-{name}"), &output)?;

        // The probe needs a runtime and a socket, which are the consumer's choices, not the
        // product's — the same appendix the example crate gets.
        let manifest_path = directory.join("Cargo.toml");
        let existing = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {manifest_path}"))?;
        std::fs::write(
            &manifest_path,
            format!(
                "{existing}\n[dev-dependencies]\n\
                 tokio = {{ version = \"1\", features = [\"rt-multi-thread\", \"macros\", \"net\"] }}\n"
            ),
        )
        .with_context(|| format!("writing {manifest_path}"))?;

        let tests = directory.join("tests");
        std::fs::create_dir_all(&tests).with_context(|| format!("creating {tests}"))?;
        let file = tests.join("probe.rs");
        std::fs::write(&file, render(&config.package.name, &plan))
            .with_context(|| format!("writing {file}"))?;

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
            println!("         written but not run: {file}");
            continue;
        }

        let run = Command::new("cargo")
            .current_dir(&directory)
            .env("CARGO_TARGET_DIR", crate::generated::shared_target())
            .env_remove("RUSTFLAGS")
            .args(["test", "--quiet", "--all-features", "--test", "probe"])
            .output()
            .context("running the probe")?;
        if !run.status.success() {
            failures += 1;
            println!(
                "probe: {name} FAILED\n{}{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
        }
    }

    if failures > 0 {
        bail!("{failures} documents have a client and server that disagree on the wire");
    }
    println!(
        "probe: every driven operation of {} documents extracts cleanly and answers with its \
         declared status",
        selected.len()
    );
    Ok(())
}

/// The probe test file: the double implementing every servable operation, then one driver test per
/// driven one.
fn render(krate: &str, plan: &Probe) -> String {
    let krate = krate.replace('-', "_");
    let mut out = String::new();
    let _ = writeln!(
        out,
        "//! Generated by `cargo xtask probe`. Every servable operation of this description,\n\
         //! sent by the generated client to the generated server over a socket.\n\
         #![allow(unused_variables, unused_imports, deprecated, clippy::all)]\n\n\
         use {krate}::{{client, server, types}};\n\n\
         /// Read a synthesized value out of its JSON spelling.\n\
         fn value<T: serde::de::DeserializeOwned>(json: &str) -> T {{\n\
         \x20   serde_json::from_str(json).expect(\"a synthesized value matches its own contract\")\n\
         }}\n\n\
         #[derive(Debug, Clone)]\n\
         struct Double;\n\n\
         async fn serving() -> client::Client {{\n\
         \x20   let router = server::router(Double);\n\
         \x20   let listener = tokio::net::TcpListener::bind(\"127.0.0.1:0\")\n\
         \x20       .await\n\
         \x20       .expect(\"an ephemeral port\");\n\
         \x20   let address = listener.local_addr().expect(\"a bound address\");\n\
         \x20   tokio::spawn(async move {{\n\
         \x20       let _ = axum::serve(listener, router).await;\n\
         \x20   }});\n\
         \x20   client::Client::new(format!(\"http://{{address}}\"))\n\
         }}\n\n\
         impl server::Api for Double {{"
    );
    for operation in &plan.operations {
        out.push_str(&handler(operation));
    }
    out.push_str("}\n");
    for operation in &plan.operations {
        if operation.skip.is_none() {
            out.push_str(&driver(operation));
        }
    }
    out
}

/// One trait method on the double: presence assertions, then the synthesized response.
fn handler(operation: &ProbeOp) -> String {
    let mut out = String::new();
    let mut arguments = String::new();
    for group in &operation.groups {
        let _ = write!(arguments, ", {}: server::{}", group.arg, group.ty);
    }
    if let Some(body) = &operation.body {
        // An optional body reaches the trait as an `Option`, exactly like an optional parameter
        // reaches its group struct.
        if body.required {
            let _ = write!(arguments, ", body: {}", in_crate(&body.ty));
        } else {
            let _ = write!(
                arguments,
                ", body: ::std::option::Option<{}>",
                in_crate(&body.ty)
            );
        }
    }
    let _ = write!(
        out,
        "\n    async fn {}(&self{arguments}) -> server::{} {{\n",
        operation.method, operation.response.enum_name
    );
    if operation.skip.is_some() {
        let _ = writeln!(out, "        unimplemented!(\"skipped by the probe plan\")");
        out.push_str("    }\n");
        return out;
    }
    for group in &operation.groups {
        for field in &group.optional_fields {
            let _ = writeln!(
                out,
                "        assert!({}.{field}.is_some(), \"`{field}` was set by the driver and \
                 dropped on the way to the handler\");",
                group.arg
            );
        }
    }
    if operation.body.as_ref().is_some_and(|body| !body.required) {
        let _ = writeln!(
            out,
            "        assert!(body.is_some(), \"the body was set by the driver and dropped on \
             the way to the handler\");"
        );
    }
    let _ = writeln!(
        out,
        "        server::{}::{}(value::<{}>({}))",
        operation.response.enum_name,
        operation.response.variant,
        in_crate(&operation.response.ty),
        json_literal(&operation.response.json),
    );
    out.push_str("    }\n");
    out
}

/// One driver test: set every parameter, send, and expect the declared status back.
fn driver(operation: &ProbeOp) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "\n#[tokio::test]\nasync fn probe_{}() {{\n    let client = serving().await;\n    \
         let response = client.{}()",
        operation.method, operation.method
    );
    for setter in &operation.setters {
        let _ = write!(
            out,
            "\n        .{}(value::<{}>({}))",
            setter.setter,
            in_crate(&setter.ty),
            json_literal(&setter.json)
        );
    }
    if let Some(body) = &operation.body {
        let _ = write!(
            out,
            "\n        .body(value::<{}>({}))",
            in_crate(&body.ty),
            json_literal(&body.json)
        );
    }
    let _ = write!(
        out,
        "\n        .send()\n        .await\n        .unwrap_or_else(|error| \
         panic!(\"the server rejected what the client built: {{error:?}}\"));\n    \
         assert_eq!(response.status().as_u16(), {});\n}}\n",
        operation.response.status
    );
    out
}

/// A type path as rendered for inside the generated crate, respelled for its integration tests.
///
/// The renderer writes `super::types::Pet` because the client and server modules are siblings of
/// `types`; a `tests/` file is outside the crate, where the same type is `{krate}::types::Pet` —
/// but `use {krate}::...` imports make the bare `types::Pet` form resolve, so `super` maps to
/// nothing at all.
fn in_crate(rendered: &str) -> String {
    rendered
        .replace("super :: types :: ", "types::")
        .replace(' ', "")
}

/// A JSON value as a Rust string literal, whatever it contains.
fn json_literal(json: &str) -> String {
    format!("{json:?}")
}
