# What the corpus answered

Measured over all 78 documents, with `cargo xtask corpus --stats` and a handful of one-off greps.
Recorded so the next stage reads a number instead of re-deriving it.

Reproduce with:

```sh
task corpus:fetch
task corpus            # round trip, references, dialect convergence, generation, snapshots
task corpus:stats      # the model-level histograms
task corpus:compile    # generate the quick tier and compile it
task differential      # the two serde renderings, asserted equivalent
```

## The front end holds every document

78/78 round-trip clean: `load → normalize → parse → serialize` returns the value it was given, by
`Value` equality, for every document in the manifest — 3.1 exactly, 3.0 modulo the documented
normalization. 535,105 schemas in total, the largest single document holding 38,269. Every document
is also parsed twice and the two models compared, and generated twice and the two outputs compared,
so the results are stable rather than merely achievable.

**What that settles.** A model that owns its own types holds real published descriptions with no
repair layer. The predecessor's ~3,700 lines of `$ref` repair, re-translation and `allOf`
reconciliation were compensating for an interchange type that could not hold the input, not for
inherent messiness in the input.

## Every reference resolves

| | count |
| --- | --- |
| schema `$ref`s | 105,242 |
| resolved as written | 105,242 |
| resolved only after a repair | 0 |
| resolved to nothing | 0 |
| addressing another document | 0 |
| `$dynamicRef` | 0 |
| document-level references to components | 64,665 |
| …of those, addressing nothing | 614 |
| groups of mutually referencing schemas | 96, the largest holding 275 |

**Not true on the first run.** 116 schema references dangled, all in `codat-accounting`, all of the
form `#/components/schemas/X/definitions/Y`. `definitions` is draft-04's spelling of `$defs`: not an
OpenAPI keyword, so the parser held it verbatim among the uninterpreted members — which left the
positions inside it with no address for a reference to resolve to. The normalizer already treated it
as a schema position, so the two phases disagreed. `definitions` is now modelled as the schema map it
is, and the 116 references resolve. The finding is worth keeping because of its shape: a keyword held
losslessly is still invisible to anything that needs to *address* it.

**The 614 that do not resolve are vendor defects**, and neither is subtle:

- `miro` writes 612 references to `#/components/responses/{code}` and declares no
  `components.responses` section at all.
- `pagerduty` points two response arms at `#/components/requestBodies/…`, which is not a response.

## The two dialects converge

`corpus/convergence/dialects.{3.0,3.1}.yaml` describe one API in both dialects — hand-written, so the
3.1 half is an independent statement of what the normalization is supposed to *mean* rather than a
recording of what it currently does. Their models are equal, member for member, with the declared
version excluded because that is the one member they are supposed to disagree about.

## Answers to the questions the front end had to settle first

| question | answer | consequence |
| --- | --- | --- |
| Do any documents use external-file `$ref`s? | **No.** 0 of 105,242 references. The raw grep looks like 59 hits, all YAML folded scalars whose content starts `#/components/…`. | External-reference support stays out of scope. The `DocumentSet` contingency is not needed. |
| Any `$dynamicRef` / `$dynamicAnchor`? | **No.** 0 occurrences. No `$anchor` either. | Dynamic-scope machinery would have zero callers. Both keywords are held losslessly, resolved as their plain form, and diagnosed on use. |
| Any non-root `$id`? | **One**, in one document. A grep finds 7, but 6 of those are inside `example` payloads rather than in schema positions — which is itself a check on the positional walk. | Base-URI computation matters, barely. Implemented, and fixture-tested for the cases the corpus does not reach. |
| Any duplicate keys? | **No.** Checked with key-preserving parsers over every JSON and YAML document. | Concern dropped. The loader keeps the last value, as every other tool does. Duplicate-*payload*-key rejection is a different question, and the hand-written deserializer does it. |
| Any non-finite numbers (`.inf` / `.nan`)? | **No** in the corpus. | Policy implemented and fixture-tested rather than corpus-tested. |
| Which YAML loader? | See below — the corpus decided it. | |

## The YAML loader, decided by evidence

Three properties are non-negotiable, and no candidate had all three for free.

1. **YAML 1.2 core-schema resolution.** `figma` declares a required property named `y` and `stytch`
   a JWK field named `n`. A YAML 1.1 loader resolves both to booleans, silently turning
   `required: [x, y]` into `required: [x, true]`. `zendesk` writes `change: =`, which YAML 1.1
   resolves to the `!!value` tag — PyYAML refuses the whole document over it.
2. **Leniency about flow-collection indentation.** `netbird` closes a multi-line `enum: [` at the
   key's own column. Strictly, flow content in a block context belongs at `n+1`; universally, every
   implementation accepts it. The pure-Rust YAML 1.2 parsers (`saphyr-parser`, `yaml-rust2`) reject
   the document outright.
3. **Exact number literals.** `1` and `1.0` are different defaults to render, and `f64` round-tripping
   loses that.

`libyaml-safer` reports the raw scalar text, its style and its tag and leaves *resolution* to the
caller, so progeny applies the 1.2 core schema itself over a lenient parse and keeps the literals.
Two costs come with it, both handled at the loader:

- It cannot finish a block scalar that runs to the end of a file with no final line break — and 38
  of 78 cached documents are served that way, 2 of them YAML. The break is supplied, and the one
  observable effect (clip chomping keeps a newline the document did not write) is diagnosed.
- It is a port of a C library and panics on a few adversarial inputs, such as an unterminated flow
  mapping whose first token is a tag indicator. The parse runs inside a panic boundary that turns
  that into an ordinary rejection, since "no input panics the generator" is the invariant that
  matters. 400,000 randomized and mutated inputs: 0 panics escaping, 45,822 accepted, 0 round-trip
  holes.

  **A caught panic is still fatal under a fuzzer, and that had to be dealt with.** `libfuzzer-sys`
  installs a panic hook that aborts, and a hook runs *before* unwinding — so the boundary never got
  to act and `cargo fuzz` reported `[!S,,-` as a crash within three minutes. The library is right
  (generation returns `Err(unparsable)` for that input); the harness was wrong. The fuzz targets now
  install a hook that reports and returns, so a caught panic costs a line on stderr while one that
  escapes still unwinds out of the target's `extern "C"` entry point and still aborts. Without that,
  no fuzz target could reach past the YAML loader at all.

## Answers the type and API models asked for

Counts over all 78 documents.

### `anyOf` in the wild — 12,934 occurrences

| pattern | count | share |
| --- | --- | --- |
| `[T, {"type": "null"}]` — nullable emulation | 10,752 | 83.1% |
| every branch a `const` or single-valued `enum` | 161 | 1.2% |
| every branch a different `type` | 754 | 5.8% |
| something else | 1,267 | 9.8% |

**Consequence.** Five sixths of all `anyOf`s are asking for `Option<T>` and have an exact
translation. Under a tenth are candidates for degradation, and the union policy is written against
the four rows above rather than against "any combination may match" in general.

### Everything else

| count | reading |
| --- | --- |
| `oneOf` 7,553, of which 763 carry a discriminator | Discriminated unions are 10% of unions; untagged is the common case, which matches the decision to leave data-carrying enums on the derive. |
| optional **and** nullable properties: 16,100 | Absent and `null` are different documents this often. Decided in [02](../plan/02-type-model.md): collapse to one `Option`, keep `Presence::OptionalNullable` in the contract, diagnose the collapse, do not build `Patch<T>`. |
| integers 32,673, of which 7,009 carry a bound (21%) | Well above the 1% that would have settled this for flat `i64`/`u64` — but the question was wrong. Narrowing a width from a bound is a forward-compatibility hazard, not an optimization. Decision: sign only. |
| request bodies with more than one media type: 1,290 | Multi-content operations are real, not hypothetical. The single-body preference table needs its `Warn` to be accurate. |
| responses declaring headers: 3,576, carrying 16,434 headers | Response headers are widespread. Raw access on the client's response value is the v1 answer; typing them has a real audience. |
| security schemes: `http` 60, `apiKey` 44, `oauth2` 15 | Three kinds, closed. A typed enum over exactly these is feasible; no `openIdConnect` appears at all. |
| `patternProperties` 13, `prefixItems` 17, `const` 2,748 | `const` is everywhere; `patternProperties` is rare enough that degrading a non-uniform one costs almost nothing. `prefixItems` is *not* rare — see the tuple correction below. |
| deepest schema nesting: 23 | The loader's 128-level nesting limit has five times the headroom it needs. |

### Correction: the draft-04 tuple count is 643, not 652

[02](../plan/02-type-model.md) recorded 652 tuple sites (`webflow` 651, `svix` 1) on the strength of
`webflow`'s diagnostic count. The diagnostics were 651, but only **642** of them were tuples; the
other nine were unrelated malformed members. The real total is **643**, and after normalization the
corpus holds **660 tuples** (643 rewritten plus the 17 already spelled `prefixItems`).

The conclusion the number was cited for is unchanged and now measured rather than estimated: tuples
are real support, not a cheap degradation, and the fixed-arity end-of-sequence check matters from
here on rather than from stage 8.

## What generation produces

| | |
| --- | --- |
| documents generated | 78/78 |
| lines of Rust | 983,029 |
| generated crates that compile | 8/8 of the reviewed quick tier, `cloudflare`'s among them: 89,740 schemas, 126,596 lines |
| generation is deterministic | every document generated twice per run, byte-compared |

### What the type layer had to say about the corpus

30,376 findings, aggregated into **549 records across 71 documents** — which is the aggregation
requirement earning its keep: unaggregated, the snapshot suite would be thirty thousand lines and
nobody would read it.

| class | occurrences | reading |
| --- | --- | --- |
| `presence-collapse` | 26,748 | Optional-and-nullable members, collapsed onto one `Option`. Larger than the 16,100 the earlier histogram reported because that count and the `anyOf` nullable-emulation count (10,752) were of the same phenomenon written two ways; this is the number of *fields* it actually affects. |
| `colliding-type-name` | 1,087 | Two positions asking for one name. Common in large documents, and the reason `names` is in the configuration. |
| `legacy-tuple-items` | 643 | The draft-04 tuple form, rewritten. |
| `dangling-ref` | 614 | All document-level, all vendor defects (see above). |
| `irreconcilable-all-of` | 442 | Branches that cannot all hold — 250 of them one `cloudflare` shape whose `allOf` intersects two unions. |
| `discriminator-edge-case` | 376 | Discriminated unions whose variants cannot be told apart structurally, degraded rather than guessed at. Consuming the tag is stage 4. |
| `unsupported-construct` | 247 | `propertyNames`, `unevaluatedProperties`, `not`, a mixed-type `enum`, a `type` naming two kinds. |
| `invalid-default` | 105 | Defaults that are not values of their own property's type. |
| `malformed-member` | 88 | Members whose value has the wrong shape, held verbatim. |
| `unknown-schema-type` | 16 | A `type` that is not one of the seven. |
| `legacy-exclusive-bound` | 9 | The 3.0 boolean bound flag in a 3.1 document. |
| `missing-final-line-break` | 1 | `urlbox`. |

**One of these started out as progeny's bug, not the corpus's.** `github`'s workflow-job payloads
write `allOf: [{type: number}, {type: integer}]`, which was reported as an irreconcilable conflict:
the merge treated JSON Schema's type names as opaque, and they are not — `integer` is a subset of
`number`, so a value that must be both is an integer. The intersection now knows the one containment
the names hide. Reading a degradation and asking "is the document really wrong?" is what the
diagnostics are for.

## The serde spike, measured

Function bodies per generated struct, counted by differencing `-Zunpretty=expanded` output at N=1
against N=11 (deterministic, so the count is valid on a loaded machine):

| | bodies per type | of those, serde's |
| --- | --- | --- |
| derive | 12 | **10** |
| hand-written | 5 | **3** |

Two of each are the `Clone` and `Debug` derives every generated type carries, identical on both
sides. The remaining figures land exactly where [04](../plan/04-render.md) predicted from the
predecessor's measurements — 9 `Deserialize` bodies plus 1 `Serialize` against 2 plus 1 — so the
compile-cost claim behind the hand-written path is confirmed under the new contract shape: **3.3×
fewer function bodies per type**.

The differential harness (`task differential`) asserts the two renderings agree on every case,
including an exhaustive 81-payload matrix over every member being present, null and absent. Three
things it found that no unit test would have:

1. **Assembling has to happen inside the visitor.** Reading the buffered members after
   `deserialize_struct` returned produces `missing field \`x\`` where the derive produces
   `missing field \`x\` at line 1 column 14`: the format attaches the position to errors that come
   out of `visit_map`, and by the time the call has returned it is too late. The buffered
   implementation now assembles inside the visitor through an `Assemble` trait.
2. **A struct is deserializable from a sequence, and the derive's error names the element count.**
   The buffered path implements the positional form too, including the detail that a member with a
   declared default absorbs its absence and the *next* member without one is the one the length
   complaint names.
3. **One irreducible divergence, reviewed and recorded.** An error raised while replaying a buffered
   member cannot name that member's offset in the input, because the format has read past it: the
   derive says `column 13` (the offending value), the buffered implementation says `column 14` (the
   end of the object). The sentence is identical; the offset is not, and buffering is what loses it —
   serde's own internally-tagged and untagged deserialization has the same property. The harness
   asserts the message strictly and the presence of *an* offset strictly, and that is the whole
   exception list.

**And the answer to the question the spike was for:** `TypeContract` needed **no new field**. Every
fact the buffered deserializer wanted — wire names, presence, arity, which members carry defaults —
was already in the record; only the renderer had to pass more of it along. The contract's shape is
the shape its one consumer wants.

## Diagnostics the corpus produces

Every finding, per document, is recorded in `corpus/snapshots/*.jsonl`, keyed by the SHA-256 of the
document it was taken from. That key is what makes snapshots workable for documents that are fetched
rather than committed: a mismatch with a *changed* hash is "upstream republished, re-baseline", and a
mismatch with an *unchanged* hash is "we regressed". Without the split, a vendor's routine
republication is indistinguishable from a bug.

Aggregation is what keeps the suite readable: a class that fires at scale — 642 tuple rewrites in one
document, 19 name collisions in another — is one record with a count and the first five locations
rather than 642 lines nobody reads.

## Drift found in the manifest

`hetzner-cloud` was recorded as 3.0 and now serves 3.1; the manifest was corrected. The corpus
runner cross-checks every document's declared version against the manifest on every run, so this
kind of rot surfaces rather than accumulating.
