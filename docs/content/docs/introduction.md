---
title: Introduction
weight: 1
---

# Introduction

progeny reads an OpenAPI 3.0.x or 3.1.x document — JSON or YAML, including the imperfect documents large real services actually publish — and produces the calling side (`reqwest`), the serving side (`axum`), and the shared type layer, so that the program and the description cannot disagree.

```sh
progeny openapi.yaml --out-dir generated/
```

## Three rules

Three properties govern the whole design:

- **Parse, don't validate.** Untrusted input becomes a typed value once, at the edge. Downstream code cannot receive malformed data because there is no shape for it to arrive in.
- **One direction.** `load → normalize → document → overrides → resolve → shape → contract → api → render`. Each conversion is total: it produces a value plus diagnostics, or it rejects.
- **Silently wrong output is the only forbidden failure mode.** Generating less, with a diagnostic, always beats generating something plausible. Every deviation from the input document is reported, and the caller decides which ones stop the build.

## What exists today

- **The lossless front end.** `bytes → Value → Normalized(Value) → Document + SchemaStore → ResolvedDocument`, and back to `Value`. The model holds everything a document says, including the keywords progeny does not interpret.
- **The type layer.** Schemas are classified into one closed set of shapes, `allOf` is merged, names are derived from document positions, structurally identical types are deduplicated, and every generated type is described by exactly one wire-contract record.
- **The types renderer**, emitting a complete crate, one module to `include!`, or an opt-in three-crate types/client/server workspace, with hand-written serde implementations by default or the derive on request.
- **The client renderer.** A params struct and one method per operation, with the URL, query string, headers and cookies built from the description's own parameter styles, and a typed response per declared status.
- **The server renderer.** An `Api` trait to implement, an `axum` router, and a rejection envelope — emitting a handler only for routes a router will actually accept.
- **The [conformance corpus]({{< relref "corpus.md" >}}).** 78 real-world published API descriptions, round-tripped, generated, compiled and linted.

## Two ways in

`progeny` is a library and nothing else: it takes bytes and a `Config` and returns rendered source as a map of path to string. It performs no I/O, so a build script can generate whatever shape it wants and write it where it likes.

`progeny-cli` is the front end for the checked-in-output workflow, and the only part that touches the filesystem. It ships the same shell under two names: `progeny`, and `cargo progeny`.

The [quick start]({{< relref "quick-start.md" >}}) shows both.

## One caveat worth knowing

Structs deserialize through a buffered hand-written implementation, which needs a *self-describing* format — one that names its members, as JSON, YAML and TOML do. If you feed generated structs to `bincode` or `postcard`, set `serde-impl = "derive-always"` and every type goes back to the serde derive. Nothing else changes: the two strategies are asserted equivalent on the wire, and they agree on every payload in the corpus.
