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
task bench:compile -- --ab --reps 3 oxide jellyfin okta   # what each rendering costs to compile
```

The last one needs a machine with nothing else on it; on a shared one it refuses rather than
reporting a number about the machine. `--max-load` raises the bar it refuses at, and every figure it
then produces carries the conditions it was taken under.

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
| `[T, {"type": "null"}]` — nullable emulation | 10,752 → **11,023** | 83.1% → 85.2% |
| every branch a `const` or single-valued `enum` | 161 | 1.2% |
| every branch a different `type` | 754 | 5.8% |
| something else | 1,267 | 9.8% |

**Consequence.** Five sixths of all `anyOf`s are asking for `Option<T>` and have an exact
translation. Under a tenth are candidates for degradation, and the union policy is written against
the four rows above rather than against "any combination may match" in general.

The first row grew by 271 in stage 4, and the growth is the finding rather than a correction: those
branches spell the null arm `{nullable: true}`, which is the only spelling 3.0 has, and until stage
4 read them as such they counted as "something else". Five sixths was already the answer; it is now
a slightly larger five sixths.

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
| lines of Rust | 980,381 (983,029 before stage 4 taught the union table what it may not do) |
| generated crates that compile | 78/78 |
| generation is deterministic | every document generated twice per run, byte-compared |

### What the type layer had to say about the corpus

31,056 findings, aggregated into **346 records** — which is the aggregation requirement earning its
keep: unaggregated, the snapshot suite would be thirty thousand lines and nobody would read it.

The "before" column is stage 3, so the two together are what stage 4 did to the corpus.

| class | before | after | reading |
| --- | ---: | ---: | --- |
| `presence-collapse` | 26,748 | 27,044 | Optional-and-nullable members, collapsed onto one `Option`. Larger than the 16,100 the earlier histogram reported because that count and the `anyOf` nullable-emulation count (10,752) were of the same phenomenon written two ways; this is the number of *fields* it affects. It **rose** in stage 4 because 288 branches that read as "constrains nothing" turned out to be null arms, which makes their properties nullable. |
| `colliding-type-name` | 1,087 | 1,042 | Two positions asking for one name. Common in large documents, and the reason `names` is in the configuration. |
| `legacy-tuple-items` | 643 | 643 | The draft-04 tuple form, rewritten. |
| `dangling-ref` | 614 | 614 | All document-level, all vendor defects (see above). |
| `irreconcilable-all-of` | 442 | 441 | Branches that cannot all hold — 250 of them one `cloudflare` shape whose `allOf` intersects two unions. |
| `wild-union` | **0** | **394** | The class that had a definition, an action, an aggregation — and **no reporting site**. Every one of these was previously emitted as an untagged enum that takes whichever branch parses first, which is the one forbidden failure mode. See below. |
| `nullable-union-branch` | — | 288 | New class. A 3.0 union branch whose only content is `nullable: true`. |
| `unsupported-construct` | 247 | 248 | `propertyNames`, `unevaluatedProperties`, `not`, a mixed-type `enum`, a `type` naming two kinds. |
| `invalid-default` | 105 | 105 | Defaults that are not values of their own property's type. |
| `discriminator-edge-case` | 376 | **93** | Discriminated unions progeny still cannot represent. What is left is 79 with a non-object variant, 7 with a variant the mapping never names, 5 with a variant used outside the union (where the tag property really is on the wire), 2 whose variants would carry the same tag, 1 declaring the tag as a non-string. |
| `malformed-member` | 88 | 88 | Members whose value has the wrong shape, held verbatim. |
| `multi-parent-discriminator` | **0** | 30 | Also previously unreported. A variant named by two unions' mappings: it joins both and carries the tag in neither. |
| `unknown-schema-type` | 16 | 16 | A `type` that is not one of the seven. |
| `legacy-exclusive-bound` | 9 | 9 | The 3.0 boolean bound flag in a 3.1 document. |
| `missing-final-line-break` | 1 | 1 | `urlbox`. |

### The 394 that stage 4 found progeny had been getting wrong

`wild-union` was in the catalogue from the start with an action and an aggregation assigned, and
nothing ever reported it: the classification only tested whether variants were distinguishable when
a **discriminator** was declared, so 394 unions that declared none were emitted as untagged enums
regardless. serde takes the first variant that deserializes, and an open struct accepts a payload
with members it does not declare — so a payload meant for a later branch was read as an earlier one
and quietly lost whatever that branch did not name. Silently wrong output, in the shape the design
documents name as the only forbidden one, produced by the gap between a catalogue entry and a call
site.

Two things found it, and neither was a test: writing the fixture that a catalogue row demands, and
reading the corpus counts for a class that should not have been zero. The catalogue now audits
itself — the class list is read out of the enum, and a class with neither a fixture nor a recorded
stage fails.

### Records are positions; types are what survives dedup

Worth stating because the two numbers look inconsistent and are not. The 376 `discriminator-edge-case`
records fell to 93, and yet the whole corpus emits only **15 internally tagged enums**. Both are
right: classification runs per *schema position*, and `okta` writes the same inline role union at
fifteen response positions, so one union that could not be represented was fifteen records. Dedup
runs afterwards and collapses the identical ones into one type. So a per-occurrence class counts
what a reader has to go and look at, which is positions — and the generated crate counts what a
consumer compiles, which is types.

The second reason the tagged count is small is the better one: most of those unions did not need
tagging at all. Their variants carry disjoint `const` tags, which the old distinguishability test
could not see because it only compared *required property names*. Seeing constants turns them into
exact untagged enums that keep every property the document declared — strictly better than tagging,
which would have taken the tag property off each variant.

### The tagged enums, checked by running one

Compiling proves a generated type *type-checks*; it does not prove serde accepts the shape, and
internally tagged enums are the corner of serde with the most runtime-only restrictions. So one was
run. `jellyfin`'s `GroupUpdate` is a nine-variant websocket union whose variants every one declared
the same nine-valued `Type` enum — which is why nothing structural ever told them apart:

```
in:   {"Type":"GroupJoined","GroupId":"abc","Data":null}
read: GroupJoined(SyncPlayGroupJoinedUpdate { data: None, group_id: Some("abc") })
out:  {"Type":"GroupJoined","GroupId":"abc"}
```

Four things that had to hold, and do: the right variant is chosen from the tag; the tag is written
back exactly once (a variant that had kept its own `Type` member would have emitted it twice);
`{"Type":"GroupLeft", …}` with byte-identical members reads as `GroupLeft`, which is the case that
degraded before; and an unmapped tag is an error naming all nine, rather than a silent pick.

### The three refinements

Each followed from reading the first run's degradations rather than accepting them, and together
they took the count from 821 to 394 without weakening the test:

- **Nested unions are flattened.** `oneOf: [A, {oneOf: [B, C]}]` accepts what `oneOf: [A, B, C]`
  accepts. Treating the inner union as one opaque branch reported an ambiguity where the question
  had not been asked yet. (821 → 738)
- **A branch that constrains nothing is a catch-all when it is last.** serde tries variants in
  order, so `anyOf: [array of Condition, array of anything]` — `sentry` writes it — loses nothing.
  Anywhere but last, the same branch swallows every branch after it. (450 → 394)
- **`{nullable: true}` alone in a 3.0 union is the null arm**, not a branch that says nothing. See
  the new catalogue row; this is the largest of the three. (738 → 450)

**One of these started out as progeny's bug, not the corpus's.** `github`'s workflow-job payloads
write `allOf: [{type: number}, {type: integer}]`, which was reported as an irreconcilable conflict:
the merge treated JSON Schema's type names as opaque, and they are not — `integer` is a subset of
`number`, so a value that must be both is an integer. The intersection now knows the one containment
the names hide. Reading a degradation and asking "is the document really wrong?" is what the
diagnostics are for — and it is the same question that turned 288 `qdrant`-style null arms from
degradations into `Option<T>`.

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

## The compile-cost claim, measured four stages early

The headline claim — hand-written serde against the derive, **−37…−46% CPU and −30…−40% peak
RSS** — was inherited from the predecessor. The body count (10 serde bodies → 3) confirmed the
*mechanism* under the new contract shape; it did not confirm the *effect*. Both renderings coexist
by `Config` design, so the effect can be measured as soon as the corpus generates compiling crates,
which is now — with only the type renderer to rework if it had not reproduced.

`task bench:compile -- --ab`, three documents, three repetitions each, A-B-B-A, `--jobs 1`, one
fresh measuring process per repetition:

| document | lines | CPU | peak RSS |
| --- | ---: | ---: | ---: |
| `oxide` | 7,219 | 6.01 → 2.21 s (**−63%**) | 690 → 343 MiB (**−50%**) |
| `jellyfin` | 8,574 | 6.15 → 2.01 s (**−67%**) | 690 → 367 MiB (**−47%**) |
| `okta` | 29,119 | 25.52 → 8.30 s (**−68%**) | 2.10 → 0.96 GiB (**−56%**) |

Run twice, independently, and the two runs agree within 4 points on every figure. **It reproduces,
and it is larger than the target range** — but the denominator is why, and the honest reading needs
it: these are **type-only** crates. The predecessor measured full client crates, where request
plumbing, builders and the response machinery are compile work the serde change does not touch. As
stages 5 to 7 add that surface the percentage should fall back toward the inherited range, and the
number to watch is the absolute saving rather than the ratio.

**Conditions, because they are part of the measurement.** This machine has 48 cores and was shared
throughout; the one-minute load average ran between 12 and 18, and one attempt was abandoned when
something else took it to 416. Three things make the figures usable anyway: variants alternate
A-B-B-A so drift cannot masquerade as a difference; a repetition whose load *rose* while it ran is
discarded rather than averaged in (7 of 36 were); and free memory never fell below 20 GiB against a
2.1 GiB peak, so no reading is a reclaim artefact. The checked-in `corpus/baseline.toml` records
kept and discarded counts, the load, and the core count beside every entry — a baseline may be
written on a shared machine, but never silently.

### Stage 4 did not change what the output costs to compile

The derive-only tree as it stood at the end of stage 3 is archived, so the "A" side of every future
comparison survives both `git clean` and any amount of generator drift. Measured against the
current tree on the same machine within the same few minutes:

| `oxide`, derive rendering | CPU | peak RSS |
| --- | ---: | ---: |
| stage 3, archived | 6.63 s | 684.9 MiB |
| stage 4 | 6.01–7.42 s across two runs | 684.3 MiB |

The spread between two runs of the *same* tree is wider than the difference between the two trees,
and peak RSS agrees to within 0.1%. Stage 4 changed which unions are representable; it did not
change what the result costs.

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
