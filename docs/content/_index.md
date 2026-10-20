---
title: progeny
type: docs
bookToc: false
---

<div class="pg-hero">
  <div class="pg-hero__text">
    <h1>progeny</h1>
    <p class="pg-hero__lead">Generate a <strong>Rust client and server</strong> from an <strong>OpenAPI description</strong> — faithfully, even when the description is imperfect, and saying out loud exactly what could not be represented.</p>
    <div class="pg-hero__cmd">progeny openapi.yaml --out-dir generated/</div>
    <div class="pg-hero__actions">
      <a class="pg-btn pg-btn--primary" href="{{< relref "/docs/introduction.md" >}}">Read the docs</a>
      <a class="pg-btn" href="{{< relref "/_index.md" >}}bench/">Benchmarks</a>
      <a class="pg-btn" href="https://github.com/romnn/progeny">Source on GitHub</a>
    </div>
  </div>
</div>

<div class="pg-badges">

[![build status](https://img.shields.io/github/actions/workflow/status/romnn/progeny/build.yaml?branch=main&label=build)](https://github.com/romnn/progeny/actions/workflows/build.yaml)
[![test status](https://img.shields.io/github/actions/workflow/status/romnn/progeny/test.yaml?branch=main&label=test)](https://github.com/romnn/progeny/actions/workflows/test.yaml)
[![crates.io](https://img.shields.io/crates/v/progeny)](https://crates.io/crates/progeny)
[![docs.rs](https://img.shields.io/docsrs/progeny/latest?label=docs.rs)](https://docs.rs/progeny)

</div>

progeny reads an OpenAPI 3.0.x or 3.1.x document (JSON or YAML) and emits the calling side (`reqwest`), the serving side (`axum`), and the shared type layer, so the program and the description cannot disagree. Every deviation from the input document is a structured diagnostic rather than silence, and the cost of compiling the generated crate is a measured property rather than folklore.

## What it does

<div class="pg-cards">
  <div class="pg-card">
    <h3>Lossless front end</h3>
    <p>The model holds everything a document says, including the keywords progeny does not interpret, so nothing is destroyed before the layers that decide what to do with it.</p>
  </div>
  <div class="pg-card">
    <h3>One type layer, both edges</h3>
    <p>A <code>reqwest</code> client and an <code>axum</code> server built on the same generated types, with the operation set as data beside them.</p>
  </div>
  <div class="pg-card">
    <h3>Diagnostics, not silence</h3>
    <p>Every repair and every degradation is a structured record with a breakage class and a JSON Pointer. The caller decides which ones stop the build.</p>
  </div>
  <div class="pg-card">
    <h3>Measured, not assumed</h3>
    <p>78 published descriptions gate every change, and the compile cost of a generated crate is <a href="{{< relref "/_index.md" >}}bench/">benchmarked on every commit</a>.</p>
  </div>
</div>

## Get started

```sh
# Install the CLI (or: brew install --cask romnn/tap/progeny)
cargo install progeny-cli

# Generate a crate from a description
progeny openapi.yaml --config progeny.toml --out-dir generated/
```

Or call the library from a build script; see the [quick start]({{< relref "/docs/quick-start.md" >}}).

## Documentation

- [Introduction]({{< relref "/docs/introduction.md" >}}) — what progeny is, and the rules it holds itself to.
- [Quick start]({{< relref "/docs/quick-start.md" >}}) — the CLI, and the library in a build script.
- [Generated code]({{< relref "/docs/generated-code.md" >}}) — packaging, the modules, and the soundness rule.
- [Diagnostics]({{< relref "/docs/diagnostics.md" >}}) — actions, breakage classes, and the deny gate.
- [Conformance corpus]({{< relref "/docs/corpus.md" >}}) — the 78 documents every change is measured against.
- [Reference]({{< relref "/docs/reference/_index.md" >}}) — the CLI and every `progeny.toml` key.
- [Benchmarks]({{< relref "/_index.md" >}}bench/) — the compile-cost history of the generated crates.
