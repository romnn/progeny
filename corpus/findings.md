# What the corpus answered

Measured over all 78 documents on 2026-07-26, with `cargo xtask corpus --stats` and a handful of
one-off greps. Recorded so the next stage reads a number instead of re-deriving it.

Reproduce with:

```sh
task corpus:fetch
task corpus:stats
```

## The front end holds every document

78/78 round-trip clean: `load → normalize → parse → serialize` returns the value it was given, by
`Value` equality, for every document in the manifest — 3.1 exactly, 3.0 modulo the documented
normalization. 535,105 schemas in total, the largest single document holding 38,269. Every document
is also parsed twice and the two models compared, so the result is stable rather than merely
achievable.

**What that settles.** A model that owns its own types holds real published descriptions with no
repair layer. The predecessor's ~3,700 lines of `$ref` repair, re-translation and `allOf`
reconciliation were compensating for an interchange type that could not hold the input, not for
inherent messiness in the input.

## Answers to the questions the front end had to settle first

| question | answer | consequence |
| --- | --- | --- |
| Do any documents use external-file `$ref`s? | **No.** 0 of 535,105 schemas. The raw grep looks like 59 hits, all YAML folded scalars whose content starts `#/components/…`. | External-reference support stays out of scope. The `DocumentSet` contingency is not needed. |
| Any `$dynamicRef` / `$dynamicAnchor`? | **No.** 0 occurrences. No `$anchor` either. | Dynamic-scope machinery would have zero callers. Both keywords are held losslessly and diagnosed on use. |
| Any non-root `$id`? | **One**, in one document. A grep finds 7, but 6 of those are inside `example` payloads rather than in schema positions — which is itself a check on the positional walk. | Base-URI computation matters, barely. It is stage-2 work on one document. |
| Any duplicate keys? | **No.** Checked with key-preserving parsers over every JSON and YAML document. | Concern dropped. The loader keeps the last value, as every other tool does. Duplicate-*payload*-key rejection remains a stage-8 requirement for the hand-written deserializer; that is a different question. |
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
  matters. 400,000 randomized and mutated inputs: 0 panics, 45,822 accepted, 0 round-trip holes.

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
translation. Under a tenth are candidates for degradation, and the union policy can be written
against the four rows above rather than against "any combination may match" in general.

### Everything else

| count | reading |
| --- | --- |
| `oneOf` 7,553, of which 763 carry a discriminator | Discriminated unions are 10% of unions; untagged is the common case, which matches the decision to leave data-carrying enums on the derive. |
| optional **and** nullable properties: 16,100 | Absent and `null` are different documents this often. A tri-state knob is worth designing, not assuming. |
| integers 32,673, of which 7,009 carry a bound (21%) | Well above the 1% that would have settled this for flat `i64`/`u64`. Worth measuring the compile cost of narrowing before choosing. |
| request bodies with more than one media type: 1,290 | Multi-content operations are real, not hypothetical. The single-body preference table needs its `Warn` to be accurate. |
| responses declaring headers: 3,576, carrying 16,434 headers | Response headers are widespread. Raw access on the client's response value is the v1 answer; typing them has a real audience. |
| security schemes: `http` 60, `apiKey` 44, `oauth2` 15 | Three kinds, closed. A typed enum over exactly these is feasible; no `openIdConnect` appears at all. |
| `patternProperties` 13, `prefixItems` 17, `const` 2,748 | `const` is everywhere; the other two are rare enough that degrading them costs almost nothing. |
| deepest schema nesting: 23 | The loader's 128-level nesting limit has five times the headroom it needs. |

## Diagnostics the corpus produces today

676 in total, across 5 documents. Every one is a vendor defect rather than a model gap.

| document | count | finding |
| --- | --- | --- |
| `webflow` | 651 | `items: [A, B]` — the draft-04 tuple form, in a document declaring 3.1. Held verbatim and degraded. **The strongest candidate for a normalization row** (`items` array → `prefixItems`) once anything consumes tuples. |
| `meilisearch` | 15 | `externalDocs.description: null` on 15 tags. |
| `openai` | 7 | `exclusiveMinimum: true` — the 3.0 boolean form, in a document declaring 3.1. |
| `influxdb` | 1 | `required: true` beside a `$ref`, the Swagger-era boolean form. |
| `svix` | 1 | `items: [A]`, as `webflow`. |
| `urlbox` | 1 | No final line break, ending inside a block scalar. |

## Drift found in the manifest

`hetzner-cloud` was recorded as 3.0 and now serves 3.1; the manifest was corrected. The corpus
runner cross-checks every document's declared version against the manifest on every run, so this
kind of rot surfaces rather than accumulating.
