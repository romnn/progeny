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

- **The lossless front end.** `bytes → Value → Normalized(Value) → Document + SchemaStore`,
  and back to `Value`. The model holds everything a document says, including the keywords
  progeny does not interpret, so nothing is destroyed before the layers that decide what to
  do with it.
- **The conformance corpus.** 78 real-world published API descriptions, round-tripped through the
  model and compared by value. 78/78 clean; what that measurement settled is written down in
  [corpus/findings.md](corpus/findings.md).
- **The infrastructure the later stages are measured by**: the corpus runner, the compile-cost
  benchmark, the module-layer lint, and two fuzz targets.

Nothing generates code yet: `generate` reads a description, reports everything it had to repair or
could not represent, and returns no files.

## Layout

| Path              | Contents                                                          |
| ----------------- | ----------------------------------------------------------------- |
| `crates/progeny/` | the generator library, and the thin `progeny` regeneration binary |
| `xtask/`          | corpus runner, compile-cost benchmark, module-layer lint          |
| `corpus/`         | the provenance manifest, the quick tier, the one committed spec   |
| `fuzz/`           | fuzz targets over the front end                                   |
| `plan/`           | the design documents this implementation follows                  |

## Working on it

```sh
mise install                # tooling
task check                  # fast inner loop
task test                   # hermetic unit tests
task corpus:fetch           # download the 78 corpus documents into corpus/cache/
task corpus                 # round-trip every one of them through the model
task lint:fc                # the lint gate: every feature combination × target
task corpus:stats           # the model-level counts the design questions turn on
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
