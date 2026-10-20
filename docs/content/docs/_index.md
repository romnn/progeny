---
title: Documentation
bookToc: false
bookFlatSection: false
---

# Documentation

progeny generates a Rust client, a Rust server, and the type layer they share from an OpenAPI 3.0.x or 3.1.x description. This documentation covers running it, configuring it, what it generates, and how it reports what it could not represent.

## Start here

- **[Introduction]({{< relref "introduction.md" >}})** — what progeny is, what exists today, and the rules it holds itself to.
- **[Quick start]({{< relref "quick-start.md" >}})** — install the CLI, generate a crate, or call the library from a build script.

## Concepts

- **[Generated code]({{< relref "generated-code.md" >}})** — packaging, the generated modules, reading responses, and the soundness rule.
- **[Diagnostics]({{< relref "diagnostics.md" >}})** — repairs, degradations and warnings, the breakage classes, and how a build refuses them.
- **[Conformance corpus]({{< relref "corpus.md" >}})** — the 78 published descriptions every change is measured against.

## Reference

- **[CLI]({{< relref "reference/cli.md" >}})** — arguments and exit status.
- **[Configuration]({{< relref "reference/configuration.md" >}})** — every `progeny.toml` key.

## Benchmarks

The compile cost of the generated crates is measured on every commit to `main`. The [benchmark dashboard]({{< relref "/_index.md" >}}bench/) charts its history.

> [!NOTE]
> **progeny is under construction.** The front end, the type layer and all three renderers exist and are gated by the corpus, but expect the generated surface to keep changing between releases.
