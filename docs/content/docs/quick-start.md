---
title: Quick start
weight: 2
---

# Quick start

There are two ways to run progeny: the CLI, which writes generated source you check in, and the library, which a build script calls to generate source at build time.

## The CLI

Install `progeny-cli` with Cargo, or as a precompiled binary from [GitHub Releases](https://github.com/romnn/progeny/releases) for Linux, macOS, and Windows. Homebrew installs either command independently:

```sh
cargo install progeny-cli

brew install --cask romnn/tap/progeny
brew install --cask romnn/tap/cargo-progeny
```

The package ships the same shell under two names, `progeny` and `cargo progeny`. Point it at a description and a directory to write to:

```sh
progeny openapi.yaml --out-dir generated/
cargo progeny openapi.yaml --config progeny.toml --out-dir generated/
```

With the default configuration this writes one crate, named `api`:

```text
generated/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── types.rs
    ├── operations.rs
    ├── client.rs
    ├── server.rs
    └── support.rs
```

Every diagnostic is printed, followed by a summary line counting what was repaired, what was degraded, and what is worth a look. A [`progeny.toml`]({{< relref "reference/configuration.md" >}}) changes what is generated and which diagnostics fail the run; the [CLI reference]({{< relref "reference/cli.md" >}}) lists every argument.

`progeny --version` reports the version of the *library* that would generate the output, not of the front end, because that is the number a reviewer of a generated diff needs.

## The library

`progeny` takes bytes and a `Config` and returns rendered source as a map of path to string. It performs no I/O, so a build script decides where the files go.

```toml
[build-dependencies]
progeny = "0.0.3"
```

`packaging = "module"` renders the whole interface as one file, `progeny.rs`, to `include!`:

```rust
// build.rs
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo::rerun-if-changed=openapi.yaml");
    let input = std::fs::read("openapi.yaml")?;
    let config = progeny::Config {
        packaging: progeny::Packaging::Module,
        ..progeny::Config::default()
    };
    let output = progeny::generate(&input, &config)?;
    for diagnostic in &output.diagnostics {
        println!("cargo::warning={diagnostic}");
    }
    if let Some(denied) = config.denied(&output.diagnostics).next() {
        return Err(format!("denied by the configuration: {denied}").into());
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    for (path, source) in &output.files {
        std::fs::write(out_dir.join(path.as_str()), source)?;
    }
    Ok(())
}
```

```rust
// src/lib.rs
include!(concat!(env!("OUT_DIR"), "/progeny.rs"));
```

The file is edition 2024 source, like the generated crates, so the crate that includes it has to be on edition 2024 and Rust 1.88 or later, and it declares the dependencies the generated code uses itself. `Output::diagnostics` is not optional: the only way to obtain generated source is to obtain the diagnostics with it. A document progeny cannot use at all — unparsable bytes, a version it does not implement, neither `paths` nor `webhooks` — is a `RejectError` instead, and there is no partial rejection.

### Numbers

The library leaves `serde_json`'s features alone, because Cargo unifies features across a build: a library that turned on `arbitrary_precision` would change how every crate beside it parses numbers. Without it, a number keeps its value but not always its spelling (`1.00` reads as `1.0`). A number the model holds only as a nearest value is reported as `inexact-number` rather than rounded silently, and one beyond an `f64`'s range is dropped and reported as `non-finite-number`. The `exact-numbers` feature keeps every literal as the document wrote it; the CLI turns it on.

### JSON Schema bundles

A protocol that publishes a plain JSON Schema bundle rather than an OpenAPI document — draft-07 `definitions`, 2020-12 `$defs`, or namespaces nested in them — is read by `generate_schemas`, with the types the caller exchanges as its roots:

```rust
use progeny::{Config, Direction, SchemaInput, SchemaRoot};

fn generate(bundle: &[u8]) -> Result<progeny::Output, progeny::RejectError> {
    let input = SchemaInput {
        // In precedence order: a name both define names the first one's type.
        namespaces: vec!["/definitions/v2".into(), "/definitions".into()],
        roots: vec![
            SchemaRoot { name: "ThreadStartParams".into(), direction: Direction::Sent },
            SchemaRoot { name: "ThreadStartResponse".into(), direction: Direction::Received },
        ],
    };
    progeny::generate_schemas(bundle, &input, &Config::default())
}
```

Only what the roots reach is generated. A root's direction stands in for the request or response position an operation would give it.
