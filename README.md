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
- **The types renderer**, emitting either a complete crate or one module to `include!`, with
  hand-written serde implementations by default or the derive on request.
- **The client renderer.** One method per operation, with the URL, query string, headers and cookies
  built from the description's own parameter styles, and a typed response per declared status.
- **The server renderer.** An `Api` trait to implement, an `axum` router, and a rejection envelope —
  emitting a handler only for routes a router will actually accept, which is asked of `matchit`
  rather than guessed from the template's shape.
- **The conformance corpus.** 78 real-world published API descriptions: round-tripped through the
  model and compared by value, references resolved and accounted for, generated twice and
  byte-compared, with every finding recorded in a hash-keyed snapshot. What those measurements
  settled is written down in [corpus/findings.md](corpus/findings.md).
- **The harnesses the later stages are measured by**: the corpus runner, the compile-cost and serde
  runtime benchmarks, the differential serde harness, the module-layer lint, and three fuzz
  targets.

`generate` reports everything it had to repair or could not represent, every time.

**One caveat worth knowing before you start.** Structs deserialize through a buffered hand-written
implementation, which needs a *self-describing* format — one that names its members, as JSON, YAML
and TOML do. If you feed generated structs to `bincode` or `postcard`, set `serde-impl =
"derive-always"` and every type goes back to the serde derive. Nothing else changes: the two
strategies are asserted equivalent on the wire, and they agree on every payload in the corpus.
On the quick-tier payload set plus a 278 KB deep fixture, buffering currently costs 3.21×/3.68×
derive wall time on valid/malformed paths, 1.89×/2.02× the allocations, and 3.47× peak heap. The
published budget caps those ratios at 4.5×, 2.25×, and 4× respectively; the full workload and
measurement conditions live in [corpus/runtime.toml](corpus/runtime.toml).

## Streams over paginated listings

Declared, never detected. 62 of the 78 corpus documents paginate and no two agree on how to say so
— the cursor parameter is `offset` 541 times, `page` 319, `cursor` 213, `after` 198 — so progeny
asks rather than guesses:

```toml
[pagination.list_pets]
cursor-param = "cursor"     # the query parameter, by its wire name
next-cursor = "next"        # where the next cursor is in the success response
items = "items"             # where the page's items are; their element type is the stream's
```

Every name is checked against the document before anything is generated, and a name that does not
resolve says what it looked for and what the document had instead. The operation then gains a
`stream()` beside its `send()` — never instead of it — and the generated crate depends on
`futures-core` and `futures-util` only because you asked for one.

## Releasing

```sh
task test:all               # unit, integration and doc tests
task lint:fc                # every feature combination × target, warnings denied
task lint:layers            # the one-directional layer rule
task typos                  # prose, excluding the vendor documents and their snapshots
task corpus                 # all 78: round-trip, resolve, generate, snapshot
task corpus:compile         # the quick tier, compiled and linted, in the default serde mode
task corpus:compile -- --serde derive        # and through the escape hatch
task payloads               # serde against the payloads the documents carry
task differential           # the two serde renderings, equivalent on the wire
task example                # the generated client against the generated server, over a socket
task probe                  # the same, generated: every servable operation of the tier
task audit && task unused   # advisories, and dependencies nothing uses
task bench:compile -- --ab --reuse --reps 6 --max-load 5 --write-baseline   # needs an idle machine
task bench:runtime -- --reps 4 --iterations 1000 --max-load 5 --write       # also needs one
```

The benchmarks are last and separate on purpose: their results depend on what else the machine is
doing, and both harnesses refuse to record a figure taken outside the shared discipline rather than
quietly writing one down.

## Layout

| Path              | Contents                                                          |
| ----------------- | ----------------------------------------------------------------- |
| `crates/progeny/` | the generator library, and the thin `progeny` regeneration binary |
| `xtask/`          | corpus runner, compile/runtime benchmarks, module-layer lint      |
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
task bench:runtime          # serde time, allocation count and peak heap; needs an idle machine
task fuzz -- front_end      # soak a fuzz target (nightly + cargo-fuzz)
```

Vendor specs are not committed: they total ~117 MB and their redistribution rights vary by
publisher. `corpus/manifest.toml` carries the provenance and `task corpus:fetch` downloads
them. The single exception is `corpus/specs/petstore-31.yaml`, which is hand-written,
hermetic, and therefore always available to offline tests.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
