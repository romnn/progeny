# progeny

Generate a Rust client and server from an OpenAPI description — faithfully, even when the
description is imperfect, and saying out loud exactly what could not be represented.

progeny reads an OpenAPI 3.0.x or 3.1.x document (JSON or YAML) and emits the calling side
(`reqwest`), the serving side (`axum`), and the shared type layer, so the program and the
description cannot disagree. Every deviation from the input document is a structured
diagnostic rather than silence, and the cost of compiling the generated crate is a measured
property rather than folklore.

## Status

Under construction. What exists today:

- **The lossless front end.** `bytes → Value → Normalized(Value) → Document + SchemaStore →
  ResolvedDocument`, and back to `Value`. The model holds everything a document says, including the
  keywords progeny does not interpret, so nothing is destroyed before the layers that decide what to
  do with it.
- **The type layer.** Schemas are classified into one closed set of shapes, `allOf` is merged, names
  are derived from document positions, structurally identical types are deduplicated, and every
  generated type is described by exactly one wire-contract record.
- **The types renderer**, emitting either a complete crate or one module to `include!`, with the
  serde derive or with hand-written implementations.
- **The conformance corpus.** 78 real-world published API descriptions: round-tripped through the
  model and compared by value, references resolved and accounted for, generated twice and
  byte-compared, with every finding recorded in a hash-keyed snapshot. What those measurements
  settled is written down in [corpus/findings.md](corpus/findings.md).
- **The harnesses the later stages are measured by**: the corpus runner, the compile-cost benchmark,
  the differential serde harness, the module-layer lint, and three fuzz targets.

The client and server renderers do not exist yet: `generate` emits the shared type layer, and
reports everything it had to repair or could not represent.

## Layout

| Path              | Contents                                                          |
| ----------------- | ----------------------------------------------------------------- |
| `crates/progeny/` | the generator library, and the thin `progeny` regeneration binary |
| `xtask/`          | corpus runner, compile-cost benchmark, module-layer lint          |
| `corpus/`         | the manifest, the quick tier, snapshots, and the committed fixtures |
| `fuzz/`           | fuzz targets over the front end                                   |
| `plan/`           | the design documents this implementation follows                  |

## Working on it

```sh
mise install                # tooling
task check                  # fast inner loop
task test                   # hermetic unit tests
task corpus:fetch           # download the 78 corpus documents into corpus/cache/
task corpus                 # round-trip, resolve, generate and snapshot every one of them
task corpus:compile         # generate the quick tier and compile it
task differential           # the two serde renderings, asserted equivalent on the wire
task lint:fc                # the lint gate: every feature combination × target
task corpus:stats           # the model-level counts the design questions turn on
task bodies                 # function bodies per type, derive against hand-written (nightly)
task bench:compile -- --crate-dir <path>   # compile cost; needs an idle machine
task fuzz -- front_end      # soak a fuzz target (nightly + cargo-fuzz)
```

Vendor specs are not committed: they total ~117 MB and their redistribution rights vary by
publisher. `corpus/manifest.toml` carries the provenance and `task corpus:fetch` downloads
them. The single exception is `corpus/specs/petstore-31.yaml`, which is hand-written,
hermetic, and therefore always available to offline tests.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
