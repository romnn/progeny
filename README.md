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
- **The types renderer**, emitting a complete crate, one module to `include!`, or an opt-in
  three-crate types/client/server workspace, with hand-written serde implementations by default or
  the derive on request.
- **The client renderer.** A params struct and one method per operation — plus one per further
  request media type the position declares (`_json`, `_multipart`, …) — with the URL, query string,
  headers and cookies built from the description's own parameter styles, and a typed response per
  declared status. A required input is a plain field and an optional one an `Option`, so a missing
  or upstream-added input is a compile error at the call site rather than anything at runtime; a
  header the description does not declare rides one request through `.header(...)`.
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
On the quick-tier payload set plus a 278 KB deep fixture, buffering currently costs 3.25×/3.71×
derive wall time on valid/malformed paths, 1.89×/2.02× the allocations, and 3.47× peak heap. The
published budget caps those ratios at 4.5×, 2.25×, and 4× respectively; the full workload and
measurement conditions live in [corpus/runtime.toml](corpus/runtime.toml).

## Two ways in

`progeny` is a library and nothing else: it takes bytes and a `Config` and returns rendered source
as a map of path to string. It performs no I/O, so a build script can generate whatever shape it
wants — including a whole workspace, which is a `Config` choice rather than a front-end one — and
write it where it likes.

```toml
[build-dependencies]
progeny = "0.0.1"
```

`progeny-cli` is the front end for the checked-in-output workflow, and the only part that touches
the filesystem. It ships the same shell under two names: `progeny`, and `cargo progeny` for the
consumers who would rather not remember a second one.

```sh
cargo install progeny-cli
progeny openapi.yaml --config progeny.toml --out-dir generated/
cargo progeny openapi.yaml --out-dir generated/
```

`progeny --version` reports the version of the *library* that would generate the output, not of the
front end, because that is the number a reviewer of a generated diff needs.

## Preserving explicit null

Optional and nullable properties normally use `Option<T>`, which cannot distinguish an omitted
property from an explicit `null`. Opt into a three-state `Presence<T>` representation globally:

```toml
preserve-optional-nullable = true
```

For a specification that does not declare a confirmed null-capable property nullable, pin a
reviewed override to its schema pointer and current declared type:

```toml
[nullability-overrides]
"/components/schemas/Patch/properties/nickname" = "string"
```

An override implies three-state presence for that property. Generation fails if the pointer no
longer exists, its declared type or shape changes, or the specification makes it nullable itself;
the entry must then be reviewed and removed or updated. Globally preserving presence also enriches
response fields because request and response bodies intentionally share one generated type graph.

## Reading responses leniently

A vendor's description is a promise the vendor does not always keep, and a client that refuses a
whole response over one `null` in one record turns a description defect into an outage. So the
generated client reads responses **leniently** by default: a member that is absent, `null`, or
not what the description declared becomes `None`; a list element that cannot be read is left out;
an enum value or a union payload the description does not list lands in the type's `Unknown`
arm with its bytes intact; a member the description never declared is kept in `extra`. Nothing
is invented and nothing is silent — every deviation is recorded, keyed by its place in the
*description* rather than in the payload, so a page of two hundred records with the same drift is
one entry naming the pointer an override would use:

```text
EmploymentPartnerApi.type: null where the description does not allow it ×14 (/components/schemas/EmploymentPartnerApi/properties/type)
```

The types a response decodes into are its **read forms**, in `types::read`. A type only a
response yields is written in that form; one a request sends as well keeps its strict form in
`types` — every required member present, enforced at compile time — and gains a read twin in
`read` only where the two shapes differ, with an infallible `From<Strict>` between them. A type
the two shapes agree on (nothing required anywhere inside) is one type, re-exported into `read`;
it reports an undeclared member and does not keep it, because the same type is what a request
sends. Every generated enum is open in both forms. A read struct's `Deserialize` is the lenient
decode with the report discarded (a read alias of a list is serde's own list reading, which
refuses the whole list over one element); the generated client keeps the report:

```rust
let page = client.list_users().send().await?;
if page.is_degraded() {
    tracing::warn!(drift = %page.degradations(), "the vendor's response drifted from its description");
}
for user in page.into_value() {
    let Some(id) = user.id else { continue };   // what the business logic needs, it checks
    …
}
```

An application that wants drift in one place rather than at every call site hands the client an
observer, which sees every degraded response with its operation, status and report, and logs or
counts it however the application likes — progeny itself logs nothing:

```rust
let client = Client::new(base_url).observe(|degraded| {
    metrics::counter!("vendor_drift", "operation" => degraded.operation).increment(1);
});
```

Every type a report can name carries its site — `read::User::SITE`, with a member's wire name
put in for a member — so asking a report about one place needs nothing copied out of the
generated source:

```rust
let id = Site { member: Some("id"), ..read::User::SITE };
if page.degradations().touches(&id) { … }
```

A response whose root is not what the description declares — an object where a list was
promised, or a body that goes on after its value — is still an error: there is no value to hand
back. A stream over a paginated listing refuses a page degraded anywhere on the path to its items
or its next cursor rather than ending early or dropping items quietly; drift inside an item does
not stop it. The generated server takes read forms at its response positions, so a mock of a
drifting vendor can answer the way the vendor does. Strict decoding, which refuses every
deviation the way the derive would, stays one setting away:

```toml
decoding = "strict"
```

or `--decoding strict` on the command line. It is a choice made when the crate is generated,
not a switch on a client: strict leaves out `types::read`, so the response types change with it.

## Client middleware

The generated client has no callback or hook system. Supply a preconfigured `reqwest::Client` for
transport-wide authentication, retry, and tracing behavior. The trade-off is explicit: generic
HTTP middleware does not automatically know progeny's operation name, so it cannot attach that
identity to each trace. Applications that need it can wrap the generated `Client` in an
application-owned type whose operation-named methods create spans before sending the generated
requests. A single undeclared header rides an individual request through its `.header(...)`.

## Operations as data

Every generation with something to call also emits an `operations` module: the description's
operation set as Rust data, rendered from the same finalized model as `client` and `server`, so the
three cannot disagree. It has no dependency and no flag, and it is emitted in all three packagings —
in a workspace it lives in `<name>-types` and both edge crates re-export it beside `types`.

```rust
use api::operations::{Method, Operation, Route};

// Every operation, in the model's order — by path template, then by method — which is stable
// across runs. Exhaustive on purpose: a table keyed on it stops compiling when the description
// gains, loses, or renames an operation.
for operation in Operation::ALL {
    println!("{} {}", operation.method().as_str(), operation.path());
}

// The routes `server::router()` registers — the operations a real router accepted — so naming
// a route the server never serves is a compile error rather than a mock that never fires.
assert_eq!(Route::ShowPetById.path(), "/pets/{petId}");
assert_eq!(Route::ShowPetById.operation(), Operation::ShowPetById);

// From what axum matched back to the route, in a middleware: the request-line method and the
// template `MatchedPath` reports. A `HEAD` request falls back to the `GET` route axum
// dispatched it to.
let route = Route::from_matched(Method::from_token("GET")?, "/pets/{petId}");
assert_eq!(route, Some(Route::ShowPetById));
```

The variant is the upper-camel stem every other generated item for the operation already uses:
`Operation::ShowPetById`, `ShowPetByIdParams`, `Client::show_pet_by_id`, and `Api::show_pet_by_id`
share one stem, and `Operation::rust_name()` is that method name — the `[pagination]` key and the
label a server `Rejection` carries. Nothing here is promised stable across revisions of the
description: a renamed `operationId` renames the variant, and every caller fails to compile, which
is the point. `Method` is progeny's own enum because the types layer has no HTTP dependency; each
edge carries a bridge — `server::http_method` to `axum::http::Method`, `client::http_method` to
`reqwest::Method`, which is the same type under another name — for keying extractors and
middleware. The router registers every template *through* `Route::X.path()` as an inline `const`,
so the reflection and the server cannot drift apart, and the wire probe drives every route through
the generated router and checks that `Route::from_matched` names the one it drove. The accessors are
`const fn`s reading a `static` table, a 1.83 compiler; every generated manifest declares the
floor the emitted source stands on, `rust-version = "1.87"`, which the shipped support runtime
already needed.

## Packaging large APIs

`packaging = "crate"` remains the default: one crate, one artifact, and one version is the simplest
shape for most descriptions. `packaging = "module"` remains the build-script form. For a large API,
or a consumer workspace whose domain crates should not inherit an HTTP stack, opt into:

```toml
packaging = "workspace"
```

This emits `<name>-types`, `<name>-client`, and `<name>-server`. The types crate has no features and
the two edge crates depend on its exact generated version, so Cargo feature unification cannot pull
client or server dependencies into a types-only consumer. Publish in dependency order: types first,
then client and server. The generated workspace README repeats its concrete names and pins.

The boundary trades time for memory. Across the eight-document quick tier, three sequential
hand-written crates take 61% more wall time than one crate, while cutting the worst rustc peak by
44–63% on every non-trivial document. Cloudflare moves from 8.82 to 3.70 GiB (−58%). Choose
Workspace when peak memory or a types-only dependency boundary matters; keep the default crate when
one artifact and the shortest clean build matter more.

## Choosing among several media types

A content position that declares more than one media type keeps them all on the calling side —
every declared request media type is its own client method — and the `media-types` table picks
which one is primary, keyed by the pointer the `multi-media-type` diagnostic prints:

```toml
[media-types]
"/paths/~1dokumente/post/requestBody/content" = "multipart/form-data"
```

At a request body the pick keeps the operation's bare name and is the one media type the generated
server accepts. At a response it picks the single type the client decodes, and the request carries
it as an `Accept` header. An entry the document cannot honour is an error, not a note.

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

| Path                  | Contents                                                            |
| --------------------- | ------------------------------------------------------------------- |
| `crates/progeny/`     | the generator library: bytes in, strings out, no filesystem         |
| `crates/progeny-cli/` | the `progeny` and `cargo-progeny` binaries, and the shell they share |
| `xtask/`              | corpus runner, compile/runtime benchmarks, module-layer lint        |
| `corpus/`             | the manifest, the quick tier, snapshots, and the committed fixtures |
| `fuzz/`               | fuzz targets over the front end                                     |
| `plan/`               | the design documents this implementation follows                    |

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
