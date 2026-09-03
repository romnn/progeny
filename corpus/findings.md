# What the corpus answered

Measured over all 78 documents, with `cargo xtask corpus --stats` and a handful of one-off greps.
Recorded so the next stage reads a number instead of re-deriving it.

Reproduce with:

```sh
task corpus:fetch
task corpus            # round trip, references, dialect convergence, generation, snapshots
task corpus:stats      # the model-level histograms
task corpus:compile    # generate the quick tier and compile it, client included
task payloads          # deserialize every example payload into the type generated for it
task differential      # the two serde renderings, asserted equivalent
task bench:compile -- --ab --reps 3 oxide jellyfin okta   # what each rendering costs to compile
```

The last one needs a machine with nothing else on it; on a shared one it refuses rather than
reporting a number about the machine. `--max-load` raises the bar it refuses at, and every figure it
then produces carries the conditions it was taken under.

**Rendering and measuring are separate commands on purpose**, because only the second one needs a
quiet machine and only the first one is pinned to a version of the generator:

```sh
task bench:compile -- --ab --generate-only oxide jellyfin okta          # capture the subject, now
task bench:compile -- --ab --reuse --reps 6 --max-load 3 --max-wait 600 --write-baseline
```

`--generate-only` writes the crates plus a `bench-rendering.toml` recording the commit they came
from; `--reuse` measures exactly those, however far the generator has moved since, and says so.
`--max-wait` is how long it will sit waiting for the machine to go quiet — five minutes is right for
a run somebody is watching, and hours are right for a take left to find its own window.

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

Since stage 5 the gate compares the **generated source** as well, which is the promise convergence
was always making: the model is an intermediate nobody receives. It also compares what each half
*gave up*, and that is the part with teeth — the source can match while one dialect was understood
less well, because a degradation that types a thing as `serde_json::Value` types it that way in both
halves once one of them decides to. The three failures are named separately: a model difference is a
normalization defect, a source difference with the models agreeing is a defect in a later stage, and
a one-sided `Degrade` is neither.

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
| lines of Rust | **4,671,513** — 979,603 of them the shared type layer and the rest the client, which stage 5 added. Before that: 983,029 at stage 3, 980,390 after stage 4's union table, 979,603 after the review pass |
| generated crates that compile | 78/78 types; the quick tier with `--all-features`, so the client half is checked too |
| generation is deterministic | every document generated twice per run, byte-compared |

### What the type layer had to say about the corpus

31,110 findings, aggregated into **353 records** — which is the aggregation requirement earning its
keep: unaggregated, the snapshot suite would be thirty thousand lines and nobody would read it.
(After stage 5 the suite holds **750** records across twenty classes; the growth is operations
arriving, and the largest single contributor was cut from 1,058 records to 16 by giving
`colliding-operation-id` the aggregation the rule had always implied it needed.)

The "before" column is stage 3, so the two together are what stage 4 did to the corpus, including
the review pass that followed it ("the 76", below).

| class | before | after | reading |
| --- | ---: | ---: | --- |
| `presence-collapse` | 26,748 | 27,037 | Optional-and-nullable members, collapsed onto one `Option`. Larger than the 16,100 the earlier histogram reported because that count and the `anyOf` nullable-emulation count (10,752) were of the same phenomenon written two ways; this is the number of *fields* it affects. It **rose** in stage 4 because 288 branches that read as "constrains nothing" turned out to be null arms, which makes their properties nullable. |
| `colliding-type-name` | 1,087 | 1,041 | Two positions asking for one name. Common in large documents, and the reason `names` is in the configuration. |
| `legacy-tuple-items` | 643 | 643 | The draft-04 tuple form, rewritten. |
| `dangling-ref` | 614 | 614 | All document-level, all vendor defects (see above). |
| `irreconcilable-all-of` | 442 | 441 | Branches that cannot all hold — 250 of them one `cloudflare` shape whose `allOf` intersects two unions. |
| `wild-union` | **0** | **453** | The class that had a definition, an action, an aggregation — and **no reporting site**. Every one of these was previously emitted as an untagged enum that takes whichever branch parses first, which is the one forbidden failure mode. 394 when the reporting site was written, 453 once the test behind it was asked in declaration order. See below, twice. |
| `nullable-union-branch` | — | 288 | New class. A 3.0 union branch whose only content is `nullable: true`. |
| `unsupported-construct` | 247 | 230 | `propertyNames`, `unevaluatedProperties`, `not`, a mixed-type `enum`, a `type` naming two kinds. It **fell** because a union that degrades takes its branches out of the generated crate with it, and a branch nothing else references stops being something to report about. |
| `invalid-default` | 105 | 105 | Defaults that are not values of their own property's type. |
| `discriminator-edge-case` | 376 | **93** | Discriminated unions progeny still cannot represent. What is left is 79 with a non-object variant, 7 with a variant the mapping never names, 5 with a variant used outside the union (where the tag property really is on the wire), 2 whose variants would carry the same tag, 1 declaring the tag as a non-string. |
| `malformed-member` | 88 | 88 | Members whose value has the wrong shape, held verbatim. |
| `multi-parent-discriminator` | **0** | 34 | Also previously unreported. A variant named by two unions' mappings: it joins both and carries the tag in neither. Reported against the variant, which is the type that loses the property; against the union it was several byte-identical records, which is what a per-occurrence class must never produce. |
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
records fell to 93, and yet the whole corpus emits only **22 internally tagged enums**, against
2,738 untagged ones. Both are
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

## The 59 the review found the first fix had left behind

`wild-union` went from 394 to **453** on a re-read of the test that produces it, and the 59 are the
same failure mode the 394 were: an untagged enum reading a payload as the wrong branch and dropping
what that branch did not name. What the first implementation asked was whether *something*
distinguished two object branches. What it had to ask is whether the branch tried **first** turns the
other's payloads down — and those are different questions, because serde takes the first variant that
deserializes:

`clerk` is the exhibit, and it is not a toy. Its JWKS list is a seven-branch `oneOf` that runs
`ed25519.PublicKey` first and `ed25519.PrivateKey` fourth. The two declare the same `kty: OKP` and
`crv: Ed25519` constants and the same required members, except that the private key also requires
`d` — the private key material:

```
in:   {"kid":…,"alg":…,"use":…,"kty":"OKP","crv":"Ed25519","x":…,"d":…}
read: PublicKey { … }        # every member it requires is present; `d` is not one it declares
out:  {"kid":…,"alg":…,"use":…,"kty":"OKP","crv":"Ed25519","x":…}
```

`d` does tell the two apart, which is why a symmetric test passed them — but it tells them apart in
the useless direction, and what falls out of the payload is the secret. Reversed, the same pair is
exact: a public key has no `d` for the private branch to find. One pair, two orders, two answers, so
a test that cannot see order cannot be right about both. Closing the earlier branch is the other
repair, and it now counts as one.

The second half of the same review: **a branch is judged by the type progeny emits for it, not by
its schema.** A branch that degrades is rendered `serde_json::Value`, which accepts every payload
each of its siblings does — so answering "object" from the keywords it still has claims a
discrimination the emitted type cannot perform. A degrading branch now counts as constraining
nothing, which makes it legal last and a swallow anywhere else, the same rule the catch-all row
already had.

Two smaller ones found the same way, both silent:

- **A bare `{"enum": [1, 2, 3]}` was `serde_json::Value`**, with no diagnostic — the `type` keyword
  was the only thing consulted, so string enumerations worked and numeric ones did not. Now `i64`.
  Both halves of that mattered: the type was lost, *and* the `Value` swallowed union siblings.
- **`enum: ["a", "b", "a"]` emitted two variants renamed `"a"`.** The second is unreachable coming
  in and indistinguishable going out, so it round-tripped as the first. `Vec::dedup` only drops
  repeats that are adjacent, and documents do not write them adjacently.

And a fifth, which the corpus contains **zero** of and which is the reason a fixture is not the same
thing as a count. A discriminated union may only consume its tag when no variant is used anywhere
the property really is on the wire — the union table has always said so, and the check walked the
edges *between shapes* to decide it. An API-surface position has no such edge pointing at it. So a
component that is both a variant of a tagged union and, by `$ref`, the schema of a `200` response
had `kind` taken off it, and the response type silently lost the member coming in and omitted it
going out:

```
components.schemas.FromFile   ← variant of Source (discriminator: kind)
paths./file.get.200.schema    ← $ref to FromFile, where `kind` is on the wire
emitted:  pub struct FromFile { pub location: String }     // and no `kind`
```

An API-surface root now counts as a use. A `components.schemas` entry still does not — being named
is not being on the wire, and treating it as one would refuse every discriminated union whose
variants a document bothered to name. The corpus has no document that does this, so **no count
moved**: 453, 93 and 34 are identical before and after. That is the finding, not a footnote to it —
a safety condition with no occurrences in 78 documents is still a safety condition, and the only
thing that can hold it is a fixture written from the rule.

None of the five was caught by the corpus compiling, the snapshots matching, or the round-trips
passing: every one of them generates code that builds and runs, and is wrong only about payloads.
What found them was generating a fixture per claim and reading what serde actually did with it.

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

Run twice, independently, and the two runs agree within 4 points on every figure. **It reproduces —
and the thing it reproduces is not the inherited headline.** These are **type-only** crates. The
−37…−46% came from full client crates, where request plumbing, builders and the response machinery
are compile work the serde change never touches. The predecessor's own **types-only** microbenchmark
came out at **−59%**, recorded in its notes as a figure that "does not transfer" to real clients.

So the honest reading of −63…−68% is: *consistent with, and slightly better than, the predecessor's
types-only figure*. It says nothing yet about the target range, because nothing has been measured
against a comparable crate. Putting the two side by side and reading the larger as clearing the
smaller is the specific error this project has already made once — a types-only number quoted as an
end-to-end one — and it is why every entry in `corpus/baseline.toml` now records its `scope`, and
why `bench-compile --check` refuses a comparison across scopes.

As stages 5 to 7 add surface the percentage should fall, and **that is arithmetic, not regression**:
the denominator grows with code the serde change does not touch. The number to watch across stages
is the absolute saving in seconds and bytes; the ratio is a fact about how much other code is in the
crate.

**Conditions, because they are part of the measurement.** This machine has 48 cores and was shared
throughout; the one-minute load average ran between 12 and 18, and one attempt was abandoned when
something else took it to 416. Three things make the figures usable anyway: variants alternate
A-B-B-A so drift cannot masquerade as a difference; a repetition whose load *rose* while it ran is
discarded rather than averaged in (7 of 36 were); and free memory never fell below 20 GiB against a
2.1 GiB peak, so no reading is a reclaim artefact. The checked-in `corpus/baseline.toml` records
kept and discarded counts, the load, and the core count beside every entry — a baseline may be
written on a shared machine, but never silently.

**And the conditions are why these are not yet the number.** The harness refuses above load 1.0 by
default; it was overridden to take these. CPU-seconds are not load-immune — memory-bandwidth
contention inflates them, by up to 2.6× at load 17 — and the largest subject is the thinnest cell:
`okta.hand-written` kept **one** repetition of three, against two for `okta.derive`. Both sides ran
at comparable load, which is what makes the row directionally sound, and the mechanism behind it is
confirmed by a deterministic count that no amount of load can move. But **−63…−68% is provisional
until it is re-taken on a quiet machine** — the discipline's load 5.00 or lower, at least three kept
repetitions — and nothing downstream should cite the figure before that. The claim these numbers
establish is "the mechanism reproduces, with room to spare", which is what the measurement was moved
forward to find out; "by 67%" is a further claim and is not yet earned.

**The harness recorded them anyway, and that was the larger defect.** The discipline was written in
[06](../plan/06-workspace-and-validation.md) — A-B-B-A, `--jobs 1`, load-gated idle machine — and
nothing enforced it, because `--max-load` served as both the operator's knob for how long to wait
and the standard a *recorded baseline* is held to. Raising the knob to get a run out silently
lowered the standard, and six entries were written at load 12.7 to 18.2 without a word of complaint.
The two are now separate: the discipline is a constant (load ≤ 5.00, ≥ 3 kept repetitions, no memory
pressure), every entry that misses it is written with its shortfalls listed, and `--check` refuses a
provisional entry as the basis of a comparison. A test asserts that the checked-in file agrees with
its own recorded conditions, which is what the old file did not — it recorded the numbers *and* the
load and never drew the conclusion.

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

**Where the archive is, because an archive nobody can find is a note.** Both sides of the stage-4
A/B — six crates, 10 MiB — sit at `/home/roman/dev/progeny-bench-stage4/`, outside the repository so
they survive `git clean`, with a `GENERATED_FROM.txt` recording the revision they came from and a
`bench-rendering.toml` that `--reuse` reads for the directory list. They are the subject the
−63…−68% claim is *about*: stage 5 added the client surface, so the same measurement taken against
today's output would answer a different question. To re-take the corrected figure, copy them back
into `target/generated/` and run `--reuse`; the recorded `scope = "types-only"` then travels into
the baseline entry, and `--check` will refuse to compare it against anything with a client in it.

## What the API model found, and what it cost

Stage 5 turned the corpus into operations. Four things it settled that no earlier stage could:

**`presence-collapse` split by position.** The class fired 27,044 times across 58 documents and said
one thing about all of them. It now says which half of the API each occurrence costs, because the
consequence differs: in a request body a caller loses the ability to *send* an explicit null — the
difference between "clear this field" and "leave it alone", which is every PATCH endpoint; in a
response body they lose the ability to *tell*; in a component no operation reaches, nothing on the
wire is affected at all. `oxide`'s 171 collapses split 35 request / 115 response / 20 both / 1
nowhere; `jellyfin`'s 906 split 231 / 231 / 403 / 41. The last bucket is the one worth noticing: a
document can collapse hundreds of properties on types nothing sends.

**A path variable inside a segment is filled, not refused.** The first implementation refused
`/Videos/{itemId}/stream.{container}` on the grounds that escaping the literal part around a
variable is a decision — and it is not: the literal is path text and the variable percent-encodes
exactly as it would alone. Refusing cost `jellyfin` six working routes for no safety gained. A
segment is a *sequence* of literal and variable pieces, and filling one is the whole-segment rule
applied piece by piece. What is still refused is a template that cannot be filled at all: unbalanced
braces, a variable named twice, a path that does not start with `/`, or a variable no path parameter
declares.

**A wildcard media type is never selected.** `jellyfin` declares `application/*+json` beside
`application/json` on 75 request bodies, and the first preference table — which ranked the JSON
family together and broke ties alphabetically — chose the wildcard. A client cannot send `*` as a
content type. Wildcards now sort last whatever they wildcard over: they are a perfectly good thing
for a document to *say* about a response and never a thing to send.

**Being declared is not being on the wire.** The tag-affordability check asks whether a variant type
is used anywhere the discriminator property really travels, and answered it with "does any
API-surface root point at this key". That counted `components.responses` and `components.parameters`
entries the document declares and no operation references — which are exactly as much on the wire as
a `components.schemas` entry, which the same rule had always excluded. Roots now distinguish a
*position* an operation sends or receives from a *name* the document merely declares. Anything an
operation does reference is reached through that operation and keeps its position there, so nothing
real is lost.

## What the client half costs, counted

A deterministic count rather than a timing, so it is valid on any machine — the same kind of
evidence as "3.3× fewer function bodies", and for the same reason.

| document | operations | `types.rs` | `client.rs` | ratio | lines per operation |
|---|---:|---:|---:|---:|---:|
| cloudflare | 3,200 | 123,931 | 588,646 | 4.7× | 184 |
| github-31 | 1,194 | 38,533 | 229,902 | 6.0× | 193 |
| jellyfin | 356 | 8,564 | 83,581 | 9.8× | 235 |
| oxide | 317 | 7,207 | 51,093 | 7.1× | 161 |

**The client is the larger half of a generated crate, by a factor of five to ten.** The per-operation
figure is stable across documents of very different shapes, which says the cost is proportionate
rather than wasteful — a builder struct, a constructor, one setter per parameter, and a `send` that
builds a URL and dispatches on status. But it moves the compile-cost question: every figure this
project has measured so far was about *types*, and types are now the minority of the output.

Three consequences, none of them acted on yet because acting on them without a measurement is what
this project's discipline exists to prevent:

- The stage-4 A/B (**−63…−68% CPU**) is a claim about type-only crates and does not transfer. Both
  sides of it were rendered and archived before the client existed, so it stays re-measurable.
- The **typestate-versus-runtime builder** question ([03](../plan/03-api-model.md) #4) now has a
  denominator: a type parameter per required field, multiplied by 3,200 operations.
- The first thing to measure at stage 8 is no longer only derive-versus-hand-written; it is what
  the client half costs at all.

## The payload gate

The first check in the project that runs serde against data rather than asking whether source
compiles — and therefore the first that could have caught any of the five stage-4 defects.

It generates a crate and then generates a test *into* that crate, one `check::<T>` call per example,
because there is no way to deserialize into a type chosen at run time. Two rules it is built with:

- **Compare against the original payload, never a second round of the type's own output.** A member
  the type drops uniformly survives an idempotence check forever. The expectation is the payload
  restricted to the members the *shape* declares — the type layer's claim about what it carries,
  checked against what the emitted Rust actually carries.
- **Carry the vendor verdict from the start.** An example that contradicts its own schema is a
  finding about the document. The verdict is computed per example rather than read back out of the
  `invalid-example` diagnostics, which aggregate per document and cap their related locations at
  five — `cloudflare` writes 29, so reading it back would have been right about the first few and
  quietly wrong about the rest.

Two expectations had to be *stated* rather than discovered as failures, and both are documented
degradations rather than defects: an optional member holding an explicit `null` comes back **absent**
(the presence collapse), and a member the schema never named comes back absent because the generated
type never carried it. Positions that cannot be checked at all — arbitrary JSON, a type spelled at
the use site, a type that captures undeclared members — are counted and printed rather than skipped
quietly, because a gate that omits silently reads as coverage it does not have.

### What it found on its first run

**60 payloads across three documents came back carrying members they never had**, all of one cause:
a schema `default` was being rendered as `#[serde(default = "…")]`. serde fills the member in on the
way *in*; the member is then written on the way *out*. So a generated client sent
`force_refresh: false` on every request where the caller had never mentioned it — turning "the caller
said nothing" into "the caller said `false`", silently, which is the one forbidden failure mode.

OpenAPI's `default` is a statement about what the **server** assumes when a member is absent. It is
not an instruction to the client to fill it in. The attribute is gone; the value is stated in the
member's doc comment instead, so nothing is dropped quietly. Nothing else is lost, because every
non-required field is an `Option` and serde reads an absent `Option` as `None` unprompted.

This is the first defect in this project found by running serde against data rather than by
compiling source, and it is exactly the shape stage 4's review predicted: it generated, it compiled,
it round-tripped its document, it snapshotted, and it was wrong about payloads.

After that fix and two corrections to the harness itself — the example check now recurses into
declared members, and the expected value for an untagged union is the payload pruned under the
*first branch that accepts it*, which is the rule serde uses — the tier stands at:

| document | checked | vendor defects | not checkable | failing |
|---|---:|---:|---:|---:|
| github-31 | 409 | 23 | 101 | **3** |
| cloudflare | 208 | 8 | 59 | 0 |
| posthog | 14 | 0 | 2 | 0 |
| okta | 12 | 0 | 9 | 0 |
| petstore-31, orb, jellyfin, oxide | 0 | 0 | 0 | 0 |
| **total** | **643** | **31** | **171** | **3** |

**The three that remained were left open and the gate left red rather than tuned green.** They have
since been read, and the section below is what they turned out to be.

### The three that were left red

All three are vendor defects, and all three had one cause on progeny's side: **the example check
recursed into a struct's members but stopped at the container edge.** An array, a fixed array, a
tuple and a map were checked for being an array or an object and nothing more — never their
elements. Every contradiction one element deep was therefore invisible, the example was called
sound, and the payload gate reported the vendor's defect as progeny's.

| where | what the document says | what its own example writes |
|---|---|---|
| `PUT /user/codespaces/secrets/{secret_name}/repositories` | `selected_repository_ids` is an array of `integer` | `["1296269", "1296280"]` — the same ids as strings |
| `POST /orgs/{org}/issue-fields` | every entry of `options` requires `name`, `color` and `priority` | three options, none carrying `priority` |
| `GET /orgs/{org}/copilot-spaces/{space_number}/collaborators` | a collaborator is a user (`actor_type: User`, plus all of `simple-user`) or a team (`actor_type: Team`, requiring `type`) | a team collaborator with no `type` — so the team branch rejects it and `actor_type` rules out the user branch |

**The recorded hypothesis was half right and worth correcting.** It said all three were untagged
unions where the harness's branch selection was more permissive than serde's. That describes the
third exactly; the first two never reach a union at all. The common cause was one level down from
where it was being looked for.

Two things followed from reading them:

- **Every container recurses now**, and the tuple length check with it — a tuple lowers to a Rust
  tuple whatever `items` says about the elements past the prefix, and serde reads one only at
  exactly that length.
- **A string enumeration now rejects a non-string.** It was returning "no mismatch", which is the
  same leniency one type further down: the shape is only ever a string enumeration when every listed
  value is a string (a mixed `enum` degrades to arbitrary JSON long before), so the generated type
  reads from a string and nothing else.

The gate is green at **643/643**, and the three moved into the column they belonged in:

| document | checked | vendor defects | not checkable | failing |
|---|---:|---:|---:|---:|
| github-31 | 409 | 26 | 101 | 0 |
| cloudflare | 208 | 8 | 59 | 0 |
| posthog | 14 | 0 | 2 | 0 |
| okta | 12 | 0 | 9 | 0 |
| petstore-31, orb, jellyfin, oxide | 0 | 0 | 0 | 0 |
| **total** | **643** | **34** | **171** | **0** |

**What this buys and what it costs.** The vendor verdict is computed from progeny's own shape, so a
stricter check tolerates more — and it can only be trusted as far as the shape is. If progeny were
wrong about a member being required, this would file its own defect under the vendor's name. That
is why all three were read in the document before the check was changed, rather than the check being
loosened until the gate went green.

**What it found in the rest of the corpus.** The three payloads were the reason to look, but the
blind spot was not `github`'s. Re-running the full corpus moved **17 documents' snapshots, and every
changed line is `invalid-example`** — 110 records added against 5 replaced, with no other class and
no round-trip touched. All 17 are "same document, different diagnostics": the hash is unchanged, so
this is progeny seeing more rather than a vendor republishing. Two checked by hand in the source
document, both real:

- `polygon` declares `settlement_date` as `type: string, format: date` and writes the nanosecond
  epoch `1753851600000000000` into it, inside an array element.
- `zoom` allows `Controller` in a five-value enum and writes `Zoom Rooms Controller`.

The 110 are spread rather than concentrated: `pagerduty` 24, `zoom` 18, `superset` 16, `polygon` 11,
then a tail of thirteen documents with six or fewer, `github` among them at 5. Note that the
manifest's `bad_examples` lists are now understated — they gate nothing (the payload verdict is
computed per example, not read from the manifest) and feed only the header count, but they no longer
describe how much of the corpus contradicts itself.

**One thing checked and found not to matter.** A `prefixItems` tuple records what the elements past
the prefix may be, and lowering drops it — the generated type is a fixed Rust tuple either way. That
would be a real narrowing if a document wrote one. Across all 78 documents `prefixItems` appears in
exactly two: `meilisearch` writes `items: false` and `airflow` writes `minItems: maxItems: 2`. Both
tuples are closed, so nothing is lost anywhere in the corpus.

### And what the compile gate found that reading could not

`jellyfin` declares a query parameter called `client`. Every generated builder has a `client` field,
so the struct declared it twice and did not compile. The builder interface now reserves `client`,
`body`, `new` and `send`; a parameter wanting one takes a suffix on the Rust side and keeps its wire
name exactly. Reserved in the API model rather than in the renderer, because which names the
interface occupies is a fact about the interface.

The same run surfaced a second, quieter defect: a deprecated operation's builder is `#[deprecated]`,
and its own `impl` block then *uses* a deprecated type — a warning in the consumer's crate, about
code the consumer did not write. An `impl` cannot itself be deprecated, so it carries the allowance.

And the gate's own report had to be fixed to find either of them: it collected the first four lines
starting with `error` or `warning` in rustc's output order, so a crate with warnings near the top
and errors near the bottom reported as four warnings — which reads as "compiles, with grumbling".
Errors now come first.

### That allowance was one of three sites, and the gate that would say so is off by default

**`compiled: 8/8 generated crates check clean` is `cargo check`, not clippy.** `--clippy` is a
separate flag, it is the only thing that denies warnings, and neither the tier gate nor CI passes it.
So the gate says "clean" about crates that are emitting warnings on every build a user runs. Armed
against one document it fails immediately:

```
$ cargo xtask corpus --only okta --compile --clippy
  ok  okta  ...  DOES NOT COMPILE
      error: use of deprecated enum `types::MtlsTrustCredentialsRevocation`; ...
compiled: 0/1 generated crates check clean
```

Ten warnings, five distinct items, **two defects**:

- **The builder accessor never got the allowance the builder's `impl` got.** `Client::extend_okta_support`
  is `#[deprecated]` and returns `ExtendOktaSupport<'_>`, which is also `#[deprecated]` — and rustc
  lints both the return type in the signature and the constructor call in the body. Being deprecated
  does not exempt an item from the lint. Four operations, eight warnings.
- **A field whose type is deprecated is not itself deprecated, and warns.** `okta` marks the
  *component* `MtlsTrustCredentialsRevocation` deprecated but not the `revocation` property that
  refers to it, which is faithful — so `pub revocation: Option<MtlsTrustCredentialsRevocation>`
  is correct and warns anyway.

The second is not `okta`'s alone. A scan of the generated crates sitting in `target/generated`
finds a deprecated named type referenced from elsewhere in **15 of them** — `cloudflare` has 17 such
types, `openai` 9, `anthropic` 8. That is a lower bound rather than a survey: it counts only the
crates a previous run happened to leave on disk.

Neither is a wrong-output defect — the crates compile, and the deprecations they carry are the ones
the documents declared. They are the other thing this project says it will not ship: a generated
crate that makes noise in a build the consumer did not write. **Both are fixed**, and the second one
taught something worth keeping:

**A field-level expectation is not enough, because the derive names the type again.** Putting
`#[expect(deprecated)]` on the declaration that names the deprecated type silences the field but not
the `Deserialize` code expanded from that field, which reports at the same span. The warning count
halves and the gate stays red. Generated internals therefore name a hidden, transparent alias whose
single deprecated reference carries the expectation. The public declaration stays deprecated, so a
consumer's own use is still linted at their site.

### And behind those, a client that did not compile at all

Arming `--clippy` on documents outside the quick tier to check the deprecation fix found something
that was never about clippy: **`weather-gov`'s generated client did not compile**, and had not for
the whole of stage 5.

```
error[E0277]: the trait bound `client::OfficeBriefingDownloadLatestError: DeserializeOwned` is not satisfied
    --> src/client.rs:3816:42
     |
3816 |                     Error::ErrorResponse(support::decode(response).await?),
```

Two answers to one question that were allowed to disagree. `error_type` counts an operation's
failure arms as *the non-2xx arms plus `default`*, and declares an enum when there is more than one.
`send` decided whether to put a decoded body **into a variant** of that enum by counting the same
arms — except it only counted `default` when `default` was not also claiming success. For every
document in the corpus those two agree. `weather-gov` writes an operation with a `302`, a `default`,
and no `2XX` at all: `error_type` saw two arms and declared the enum, `send` saw one and decoded the
body straight into it — handing an enum that derives `Debug, Clone` and nothing else to serde.

`default` is a failure arm whenever it exists; claiming success as well does not stop `_ =>` from
decoding through it. The two now count identically.

**What this says about the gate is worth more than the fix.** The stage-5 gate is "clients for the
full corpus compile", and what runs is `--quick --compile`: eight documents. `weather-gov` is not
one of them, the shape appears in **1 of 78 documents and 2 operations**, and nothing else in the
corpus has it — verified by walking every `responses` object in the manifest, plus compiling
`zendesk` by hand because its YAML would not parse for the walk. A one-in-seventy-eight shape is
exactly what an eight-document tier is blind to, and it took an unrelated errand to find this one.

### What `--clippy` finds once the deprecations are gone

With `okta` green on deprecations, three lint classes it was hiding come into view — all
pre-existing, none of them about deprecation:

| lint | count in `okta` | what it is |
|---|---:|---|
| `doc_lazy_continuation` (quote) | 34 | a vendor's multi-line blockquote in a `description`, whose continuation lines carry no `>` |
| `doc_lazy_continuation` (list) | 25 | the same, for list items |
| `large_enum_variant` | 3 | a union whose variants differ enough in size that clippy wants a `Box` |

The first two are the same defect: **vendor prose is transcribed into doc comments verbatim**, and
markdown that was fine in a description is not always fine in rustdoc. The third is a design
question rather than a transcription one, because boxing a variant changes the type the consumer
gets.

None is a reason to leave `--clippy` off — they are reasons it has never been on.

### `--clippy` is the gate now, and what it took to get there

`corpus:compile` passes `--clippy`, so CI denies warnings in generated crates. Surveying the whole
quick tier first — rather than fixing what `okta` happened to show — turned up seven classes, and
they split cleanly into defects and decisions.

**Four were progeny's own output, and are fixed:**

| what | where | fix |
|---|---|---|
| `tabs_in_doc_comments` | `cloudflare` 78, `github` 4 | tabs expand to spaces; rustdoc does not define their width |
| `doc_lazy_continuation` | `posthog` 79, `orb` 62, `okta` 59, `github` 25, `cloudflare` 11 | lazy list and blockquote continuations are written out explicitly |
| `doc_overindented_list_items` | `posthog` 8 | the same rule from the other side — a continuation belongs at its item's content column |
| `single_char_add_str` | `cloudflare` 9 | a one-character path literal is `push`, not `push_str` |
| `deprecated` | `cloudflare` 2 | the client names deprecated schema types too — a live operation with a deprecated `feedback` parameter put one in a field and a setter |

**Vendor prose is transcribed, never rewritten.** What changed is that lazy continuations — a
paragraph line inside a list item or blockquote that leaves out the indent or the `>` — are now
written in the explicit form CommonMark defines them to be equal to. Nothing renders differently.
The case that makes it concrete is `posthog`, which describes an endpoint with a parenthesis that
wraps onto a line beginning `+ the spec it derived…`: markdown reads that as a list item and every
line after it as a lazy continuation of one. One habit, 79 warnings.

**It took three passes, and the two failures are the more useful half.** Writing the normalizer was
the easy part; being right about markdown was not, and the gate caught both mistakes on documents no
amount of reading would have suggested.

- **A nested list is indented relative to its parent, not to column zero.** `orb` writes a sub-item
  at column 4 under an item whose content starts at column 2. Read absolutely, four spaces is an
  indented code block; read against its parent it is a list. The first reading flattened the
  sub-item to column 2 and left its own continuation stranded at 6. What gave it away is that
  `orb`'s 62 warnings **changed class** rather than disappearing — `without indentation` became
  `overindented`, which is the signature of a normalizer moving lines to the wrong column rather
  than one leaving them alone.
- **A blank line ends the paragraph, not the list item.** `okta` writes an item, a blank line, a
  second paragraph still inside the item, and then wraps that paragraph lazily back to column zero.
  Tracking "what block is open" and "is a paragraph open" as one flag loses the item at the blank
  line and leaves everything after it unindented. They are two pieces of state.

**And a fourth defect that was not the vendor's prose at all.** progeny appends its own sentence to
a parameter's documentation — `Sent as the \`limit\` query parameter.` — directly after the
description, with no paragraph break. When a description *ends inside a list item*, markdown reads
progeny's own sentence as a continuation of it. The struct documentation and a documented `default`
already separate themselves with a blank line; the setters did not. This one is worth noting
separately because it is the only one where the markdown progeny emitted was its own.

**Three were decisions, and each now has an explicit, stable rule:**

- **`large_enum_variant`** asks for a `Box`, but dependency-defined layouts make a
  threshold-dependent choice unstable. Every non-unit payload is therefore boxed unconditionally.
  The public shape no longer changes when a configured format or map implementation changes size.
- **`type_complexity`** asks for an alias, and progeny would have to invent its name. Every name in
  the output comes from the document; a named type the document never mentions is worse than a long
  one that says exactly what the schema said. The renderer mirrors Clippy's scoring and emits a
  field-level `#[expect]` only when that exact field crosses the threshold.
- **`match_overlapping_arm`** is reporting the contract. OpenAPI says an exact status claims a
  response before a range does, so a document declaring both `400` and `4XX` produces arms that
  overlap by construction. The lint reads the rule as a mistake.

Every remaining suppression is a narrowly scoped `#[expect(..., reason = "...")]` on the construct
that requires it. There are no crate-wide lint allowances in generated output.

### What it does not cover, said plainly

The gate runs serde against **bodies**. It does not send a request. Nothing in this project yet
checks that a generated `send()` builds the URL, query string, headers and cookies a document
describes — the style table is unit-tested row by row in `support/style.rs`, and the wiring from a
parameter to the right row is checked only by reading the emitted source. That gap closes at stage 7
with the example crate, which is the first thing that will have both halves of a real request. Until
then, a claim about a request line is a claim no gate is making.

## The rest of the body surface, sized before it was built

Stage 6 is four items — typed multipart, form-urlencoded, binary and streaming bodies, cookie
parameters — and the corpus was asked how big each one is before any of them was written. Request
bodies across the 76 documents this query could parse:

| media type | bodies | documents |
| --- | ---: | ---: |
| `application/json` and the `+json` suffix family | 5,958 | 65 |
| **`multipart/form-data`** | **278** | **27** |
| **`application/x-www-form-urlencoded`** | **109** | **11** |
| `text/plain` | 14 | 5 |
| `application/octet-stream` | 13 | 4 |
| a wildcard and nothing else (`*/*`, `image/*`) | 9 | 3 |

Three things the table decided:

- **Multipart is not a footnote.** It is the largest non-JSON body by a factor of two and a half,
  and it reaches more than a third of the corpus. **236 of the 278 are an object with declared
  properties**, so the type layer already renders exactly the struct the parts come from — the work
  is a writer, not a type model.
- **`encoding` is nearly unused, and what it says is narrow.** Thirteen multipart bodies declare it,
  and the only key they use is `contentType` — 27 uses, of which the plurality are
  `application/json` on a structured member. One form body in the whole corpus declares `encoding`,
  setting `style` and `explode`. So the specification tables here are for the exceptions, and the
  defaults carry everything else.
- **Cookie parameters were already done**, from stage 5. Counting first is what showed that; the
  item stayed on the list from a plan written before the location existed.

**A form body is a query string in the body position**, so it is encoded by the same style table
rather than by a second encoder — one place where the array rules live, and the row-by-row tests
already existed. The one document that declares `encoding` on a form body selects a row through the
same classifier a query parameter goes through.

**The multipart boundary is scanned, not drawn.** A random boundary is wrong with some probability,
and the failure is a body the server silently reads as several parts — the one forbidden failure
mode. Scanning the content for a boundary that does not occur in it is correct by construction, and
it makes a generated request reproducible, which is what makes it testable at all.

### What the parts table costs, and why it is a table

One `const` slice per operation and one loop in the shipped support module, rather than a call per
part unrolled into every `send()`. The same trade the style table makes, and it matters here for the
same reason: `langsmith` writes an 18-member multipart body, and unrolling it would put 18 statements
in a method that is otherwise five.

The table says what the document was **specific** about; it is not the list of members. An
`additionalProperties` member or a flattened one is real and absent from it, so the writer walks the
value and consults the table, never the other way round.

### An array is its item's kind, which is 3.1's rule and not 3.0's

3.0 said any array member is `application/json`. 3.1 says "the default is defined based on the type
of the item". progeny follows 3.1 for both dialects, deliberately: a repeated member written as
several parts under one name is what every multipart parser expects, and 3.0's reading would put a
JSON array where a server is looking for several fields. `anthropic` writes `files` as an array of
binary strings, which under 3.0's rule would be one JSON array of file contents.

Whether a member is repeated is carried **beside** its kind rather than folded into it, because they
answer different questions — what one part holds, and how many parts there are. A member typed as
arbitrary JSON that happens to hold an array at run time is one part, because nothing *declared* it
repeated.

### 110 occurrences of a 3.0 spelling inside documents that declare 3.1

`format: binary` was removed in 2020-12; the fact moved to `contentMediaType`. **15 documents that
declare 3.1 write it anyway — 110 occurrences**, led by `telnyx` (27), `openai` (24) and `langsmith`
(18). This is the boolean-`exclusiveMinimum` situation exactly, and it gets the same answer: repair
it, diagnose it, and do not version-gate a repair that helps everybody.

It matters more here than the type it produces, which is `String` either way. In a multipart body
`format: binary` is the only thing marking **which property is a file** — a per-property fact the
media-type key cannot carry. Left unread, those members become text parts: a request built wrong,
not a type named oddly. Found by writing a fixture with `format: binary` in a 3.1 document and
watching it classify as text.

### A wildcard media type follows its schema

Nine bodies declare a wildcard and nothing else. `Content-Type: */*` is not a content type, and
`preference` only sorts a wildcard last — which decides nothing when it is the only entry, so those
nine were being sent with a header no server can act on.

Which content type to send instead is decided by the **schema**, because a wildcard permits all of
them and only one matches what the document typed. `telnyx` writes `*/*` over a `$ref` to a real
object — sending that as bytes would discard a type the document supplied — and `jellyfin` writes
`image/*` over a binary string, where bytes is exactly right. Reading the documents is what separated
the two; the first pass sent both as bytes.

### The limitation, stated rather than discovered later

A `format: binary` member renders as `String`, because inside a JSON payload a binary property *is* a
string and the type layer has no position to tell it otherwise. In a multipart part the bytes of that
string are what goes on the wire, which is faithful — but **a part whose content is not valid UTF-8
cannot be constructed**. Lifting that would mean a type depending on the position it is used in,
which forks a component type shared with a JSON body, so it is a limitation this design accepts
rather than a defect it has yet to fix.

## The serving side, and the router's rules read rather than guessed

**Route collisions are rare and they are one idiom.** 21,764 operations over 15,015 path templates
in the 78 documents, and **exactly two of them** contain a same-shape collision: `polygon` (31
operations) and `miro` (7). Both do the same thing — disambiguate a path by renaming its parameter,
which changes the documentation and not the URL. `miro` writes `{board_id}` and
`{board_id_PlatformFileUpload}` for one route; `polygon` writes `{cryptoTicker}`, `{fxTicker}`,
`{indicesTicker}`, `{optionsTicker}` and `{stockTicker}` for one indicator endpoint, five times over.

A colliding route **keeps its client method and loses its server handler**. That is the
position-degrades rule applied to a new position: a client builds a URL and sends it, and only a
*router* has to tell two routes apart. Deleting a working client method to fix a server's problem
would be a worse trade than the one the collision forces.

### Asking the router beat modelling it, and the corpus proved it twice

The plan called for a classifier that decides registrability from a template's shape. Two
measurements said that would have been wrong:

- `matchit` — the router `axum` matches with — **accepts** `/Videos/{itemId}/stream.{container}` and
  **refuses** `/Videos/{itemId}/Trickplay/{width}/{index}.jpg`. A parameter may have literal text
  before it in its segment, may have none after it, and may not share a segment with another. None
  of that follows from anything OpenAPI says, and all of it is fine by the client's fill rule.
- The rule **moves between patch releases**. A scratch probe against `matchit` 0.8.6 said
  `/a/{x}.jpg` registers; the 0.8.4 that `axum` 0.8.9 actually resolves says it does not. A model
  written from the first probe would have been a claim about a router nobody in this workspace runs.

So progeny inserts each template into a real `matchit::Router` at generation time and believes the
answer, and `matchit` is on the generator's dependency list for that one reason. **216 operations
across 10 documents** are refused outright, which is 1% of the corpus and four idioms:

| document | operations | what it writes |
| --- | --- | --- |
| `twilio-api-v2010` | 99 | `.json` after the variable — `/Accounts/{AccountSid}/Calls/{Sid}.json` |
| `anthropic` | 41 | a query string inside the template — `/v1/agents/{agent_id}?beta=true` |
| `exoscale` | 33 | an action suffix — `/block-storage/{id}:attach` |
| `mongodb-atlas` | 23 | the same — `/clusters/{clusterName}:pinFeatureCompatibilityVersion` |
| `telnyx`, `weather-gov` | 7 each | `.json` again; and `{x},{y}` as one segment |
| `cloudflare`, `frankfurter` | 2 each | `{scan_id}.png`; and a date range as `{start_date}..{end_date}` |
| `jellyfin`, `miro` | 1 each | `{width}/{index}.jpg`; and a stray `?` in the template |

**All 216 come back with the same `matchit` message**, "Only one parameter is allowed per path
segment" — which is not accurate for most of them. `/screenshots/{scan_id}.png` has exactly one
parameter in each of its segments; that message is `matchit`'s blanket answer for a parameter that
does not *end* its segment. The diagnostic therefore **attributes** the reason to `matchit` instead
of asserting it. A module that deliberately declines to model the rule is in no position to phrase
the refusal better than the router did, and quoting a router's confusing message as though it were
progeny's own reading of the document would be the worse of the two failures.

The direction of the remaining risk is stated rather than hidden: a consumer on a newer `matchit`
may find progeny was conservative, which costs a route named in a diagnostic. The other direction is
a server that panics at startup, which is the failure the classifier exists to prevent.

### What the type system of the generated crate carries

- **A handler cannot answer with an undeclared status**, because the response enum has no variant
  for one. `default` is the exception that proves it: that variant carries its own `status`, because
  `default` means "any status this description did not otherwise claim" and only the handler knows
  which one it is sending. Picking a number there would have been progeny deciding what an
  operation's unlisted statuses are.
- **A handler cannot exist for a route the router refused**, because `RegistrableRoute` has no
  public constructor and the trait method is only emitted for an operation that has one.

The first of those has an edge the corpus was asked about rather than assumed: an operation that
declares *no* responses at all would get a response enum with no variants, which compiles and leaves
a trait method nobody can return from. **0 of 21,764 operations declare none** — 3.0 requires
`responses` and 3.1 only permits omitting it — so this is recorded as measured-absent rather than
guarded against. It is also a loud failure if it ever appears: the consumer cannot write the handler,
which is the opposite of the silent kind.

### The body limit is a ceiling, and `axum`'s knob does not move it

A generated server reads at most **2 MiB** of request body. That is a decision — a server must not
become a denial-of-service target because a description said `type: string` — but the first version
of the comment above it was wrong in a way worth recording, because it told a consumer to turn a
knob that does nothing. `axum`'s `DefaultBodyLimit` works by inserting an extension that only
extractors calling `with_limited_body` consult; the generated support code reads the body with
`to_bytes` and its own constant, so the extension never comes into it. Raising `DefaultBodyLimit`
changes nothing, and a 3 MiB upload comes back as a 400 either way.

Stated rather than fixed here: making the limit a configuration knob means templating the support
module per crate, which is stage 9's kind of work. What stage 7 owes is that the number is written
down, and that no comment claims a way to change it that isn't one.

## The example crate, and the two defects it found in its first minute

Every other gate in this project checks one side of the wire: the corpus checks a document, the
compile gate checks that emitted source is Rust, the payload gate runs serde against bodies.
**None of them sends a request.** The example crate generates the client *and* the server of the
committed petstore, implements the server's `Api` trait with a double that records what arrived,
starts it on a real socket, and calls it with the generated client.

It failed immediately, twice, and both were real:

- **Every path parameter arrived as "not sent".** The extractor read them out of the request's
  extensions looking for `axum`'s `Path` type; what a router stores there is its own private type,
  so the lookup found nothing and every request was rejected for being exactly right. Reading them
  through `RawPathParams` — the extractor `axum` provides for this — is the fix.
- **A URL carries no types, and nothing was supplying them.** `?limit=3` is three characters; the
  schema is the only thing that says the value is a number. Handing serde the text a parameter
  arrived as fails for *every non-string parameter in every description*. The reading is now
  attempted as-is and only then re-read as the scalar the text spells, which is what keeps a
  `type: string` parameter holding `"9"` or `"007"` the string it was.

Neither would have been found by reading the emitted source, and neither is visible to any gate that
does not send a request. That is the whole argument for the example crate in two bullets.

## The hand-written serde path, first shown the corpus

Until stage 8 the hand-written `Deserialize`/`Serialize` renderer had met exactly **one 56-line
fixture**. All 78 corpus documents generated with the derive, the differential harness compared the
two strategies on that fixture, and nothing else had ever asked the hand-written path a question.
The renderer, the eligibility function and the buffering machinery had all been written and tested;
what had not happened is the thing this project keeps finding to be the difference — *running it
against the corpus*.

`xtask corpus --serde hand-written` and `xtask payloads --serde hand-written` exist so that both
strategies are reachable from the gates that run 78 documents. **An escape hatch nobody runs is not
an escape hatch**, and the same is true of the path that is supposed to become the default.

### The first run failed on all eight tier documents, in four ways

Every one is a warning rather than a type error, and every one would have landed in a consumer's
build as a complaint about code they did not write. That is why the compile gate denies warnings.

| what it was | where | why it only appears here |
| --- | --- | --- |
| `let mut count` with nothing to increment | `petstore-31`, `posthog`, `oxide` | a struct whose members are all required has no conditional arm |
| deprecated uses inside the impls | `github-31`, `cloudflare`, `okta`, `orb`, `jellyfin` | generated internals need non-deprecated aliases and precise member expectations |
| `unused variable: buffer` | `cloudflare`, `github-31`, `okta` | a struct with no members reads nothing |
| `let mut state` with nothing to write | the same three | and writes nothing |

The deprecation row has **three distinct shapes and the corpus produced all three**: a deprecated
*type*, which `impl Serialize for …` uses simply by naming it (`cloudflare`); a deprecated *member*,
used by reading or writing it (`jellyfin`, `github-31`); and a member whose *type* is deprecated
(`okta`). Hidden aliases cover generated type paths, while statement-level expectations cover the
exact member reads and writes.

The last two rows are one shape — **an object with no properties** — and three documents declare
one. It is now in the differential fixture, because a shape whose whole difficulty is that it
renders to code with nothing in it belongs in the gate that runs in seconds rather than the one that
compiles eight documents.

### The arity bug the plan predicted, present in the code

[04](../plan/04-render.md) specified "explicit end-of-sequence checks after replaying buffered
content — the fixed-arity (`[T; N]`, tuple) trailing-element bug class". It had not been
implemented. Replaying a buffered `Content::Seq` handed serde's `SeqDeserializer` straight to the
visitor, skipping the `end()` call that serde's own `deserialize_any` makes, so:

```rust
let content = Content::Seq(vec![U64(1), U64(2), U64(3)]);
<(u64, u64)>::deserialize(ContentDeserializer::new(content))  // Ok((1, 2)) — the third vanished
```

**Only the buffered path can have this bug**, which is why it survived: read the array directly and
the *format* notices the leftovers — `serde_json` answers with a trailing-characters error. Buffering
makes progeny the format, and a visitor asked for a fixed arity stops as soon as it has that many
and never looks again. Silently dropping input is the one failure mode this project forbids, and the
derive rejects the same payload, so the two strategies disagreed on the wire.

Found by reading the plan's specification against the code rather than by a gate, which is worth
recording as a limitation of the gates: the differential fixture had no fixed-arity member, so
nothing could have caught it. It now has one.

## Pagination: ubiquitous, and no two documents agree

Open question 1, measured across all 78 documents. **62 of them declare a cursor-ish query
parameter on a `GET`**, so pagination is not a niche — and nothing whatever about it generalizes:

| the cursor parameter is called | times |
| --- | --- |
| `offset` | 541 |
| `page` | 319 |
| `cursor` | 213 |
| `after` | 198 |
| `before` | 133 |
| `from` | 98 |
| `page_token` | 84 |
| `page[size]` | 75 |
| `Page` | 61 |
| `PageToken` | 61 |
| `page[cursor]` | 60 |
| `start` | 39 |

Four different conventions (offset, cursor, page number, opaque token), two casings, and bracketed
forms borrowed from JSON:API. The response side is no better: `next`, `next_page`, `total_count`,
`has_more`, `next_cursor`, `totalCount`, `hasMore`, `NextToken`, and a `Hasmore` that is somebody's
typo. **`Link` headers are two documents** — `github-31` with 205 operations and `okta` with one —
so RFC 5988 is not this corpus's answer whatever its reputation suggests.

**So detection would be a table of vendor spellings pretending to be a rule**, which is what the
predecessor built and what did not generalize. Pagination is *declared*, per operation, and every
name in the declaration is checked against the document before anything renders: a cursor parameter
the operation does not have, a member path that does not resolve, an `items` path that is not a
list, a next cursor that is not optional — each is a refusal that names what it looked for and what
the document had instead.

Two constraints fell out of writing it, and both are refusals rather than guesses:

- **The next cursor must be optional.** Its absence is the only thing that ends the stream. Stopping
  on an empty string or an empty page are conventions the declaration did not state.
- **The operation must have exactly one success status.** With two, the client hands back an enum,
  and picking which variant carries a page is a decision the document did not make.

The honest price: a consumer of `github-31` who wants streams writes 205 declarations. That is what
refusing to guess costs, and it is why the plain `send` is never replaced — only joined by a
`stream` beside it. The generated crate takes `futures-core` and `futures-util` **only when some
operation declared pagination**, on the same rule the client and server halves already follow.

## The wire probe, and the startup panic it found in its first hour

The example crate's lesson — only a gate that sends a request catches request-line defects — had
one document's worth of coverage. The probe is its generated form: from the same frozen contracts
the renderers read, `xtask probe` synthesizes a value for every parameter, body and response of
every servable operation, generates the recorder double and the driver tests, and runs the
document's client against its server over a socket. Per operation it asserts that a request with
every declared parameter set **extracts cleanly**, that every optional parameter **arrives**, and
that the declared response **decodes back with its declared status**. Anything unprobeable is a
named skip, counted out loud.

Its first three documents were petstore (6/6, the hand-written example's subject), oxide (**308
operations driven**, 9 named skips), and jellyfin — where it found three product defects in one
run:

- **Generated servers could panic at startup.** The registrability classifier kept one `matchit`
  router *per method*, on the stated belief that axum does the same. It does not: `axum::Router`
  keeps a single path tree, and method dispatch happens inside a matched node. So `jellyfin`'s
  `GET …/Subtitles/{language}` and `POST …/Subtitles/{subtitleId}` — different methods, same
  shape — passed the classifier and panicked the real router, the one failure the classifier
  exists to make impossible. The corrected classifier mirrors axum exactly: one router, one
  insertion per distinct template, methods merging on identical strings. **35 operations across 11
  documents** turn out to collide cross-method; every one was previously a server that could not
  boot. "Asked, not modelled" applies to the router's sharding too — the sharding was still a
  model, and it was wrong.
- **Arrays could not be read back from a query string.** `ids=a,b` under `style: form,
  explode: false` is byte-identical to a scalar containing a comma, and the reader worked from the
  style row alone — so joined arrays arrived as one string and were rejected by the typed read, and
  a single-element exploded array was indistinguishable from a scalar. The schema's shape now rides
  along into the reading: the schema decides *whether* it is an array, the row decides *how* one is
  spelled. A URL carries no types; the schema supplies them — the project already knew this rule,
  and had applied it to every location except this one.
- **`HEAD` responses were decoded.** A `HEAD` response has no body on the wire — the transport
  strips whatever the handler wrote — so the schema a document declares there documents the `GET`
  twin. The client decoded it anyway and failed on every `HEAD` in `jellyfin`. Response arms of
  `HEAD` operations now carry `()`: the statuses stay, the phantom payload goes.

After the fixes: **jellyfin 362/362 driven, 0 skipped.** One real limitation surfaced and is
recorded rather than patched: an *exploded form object* in the query writes each member as its own
key, so the parameter's name never reaches the wire and the generated server's read-by-name cannot
ever see it. The probe neither sets nor asserts such parameters; giving the degradation its own
diagnostic is filed work.

The rest of the tier held two more, each a one-source-of-truth failure between the halves:

- **`orb`: the shape flag and the rendered type came from different layers.** `status[]` is a
  *nullable* array, which classifies as a union rather than as `Shape::Array` — so the parameter's
  shape said "primitive" while its rendered type was `Option<Vec<String>>`, and the server, told it
  was reading a scalar, rejected the very requests its own client builds. The shape is now derived
  from the type the extraction decodes into, looking through the wrappers that do not change what
  the wire carries. Six operations, all clean after.
- **`posthog`: the reader had never learned the writer's one spelling for a compound.** The client
  encodes a struct-typed query element as its JSON — `properties=` carries one JSON-encoded filter
  per occurrence — and the server's second-attempt coercion stopped at numbers and booleans, on the
  written-down reasoning that "a string that happens to spell an object is a string". That
  reasoning missed what the writer actually does. The coercion now inverts it exactly, one compound
  level deep, and the safety argument is unchanged: the as-is reading runs first, so a
  `type: string` member keeps its braces.
- **`posthog` again: form bodies had no member shapes.** The `FormSpec` table carried only the
  members the document's `encoding` named — one body in the whole corpus — so a form body's reader
  fell back to guessing arrays from key repetition, and a *one-element* array member is one
  occurrence: byte-identical to a scalar, handed to serde as its element. The table is now derived
  from the body's own contract, one spec per declared member with its shape, exactly the way query
  parameters get theirs; the repetition heuristic survives only for `additionalProperties` members,
  which have no declared shape anywhere.

The tally for the probe's first day, across seven documents: **4,983 operations driven** and six
product defects found, none of which any gate that does not send a request could have seen. Five of
the six are one lesson wearing different clothes: *the wire under-determines the value, and every
reader needs the schema's answer threaded to it* — the same rule the example crate's coercion fix
established at stage 7, rediscovered at five more positions.

## Non-JSON responses, and the streaming boundary

The response contract used to collapse every selected non-JSON media type to `Vec<u64>` and route it
through JSON at both transport edges. A census over all generated response arms — after reference
resolution and the same media-type preference the generator applies, including `default` arms —
finds **598 selected non-JSON arms across 35 of 78 harness documents: 441 text and 157 bytes**. The
committed petstore wire fixture contributes one of each; the 77 published descriptions therefore
contribute **596 arms across 34 documents: 440 text and 156 bytes**.

| document | text | bytes | document | text | bytes |
| --- | ---: | ---: | --- | ---: | ---: |
| airbyte | 2 | 1 | aiven | 3 | 5 |
| anthropic | 0 | 6 | chroma | 2 | 0 |
| clickhouse-cloud | 4 | 1 | cloudflare | 22 | 22 |
| codat-accounting | 0 | 9 | deepl | 1 | 1 |
| discord | 1 | 1 | dub | 0 | 1 |
| github-31 | 3 | 1 | gladia | 0 | 3 |
| hubspot-contacts | 0 | 13 | influxdb | 3 | 1 |
| intercom | 1 | 0 | **jellyfin** | **350** | **31** |
| lago | 1 | 0 | lithic | 1 | 0 |
| meilisearch | 1 | 0 | mongodb-atlas | 0 | 7 |
| netbird | 1 | 0 | okta | 1 | 0 |
| openai | 0 | 2 | openrouter | 0 | 3 |
| opensearch | 22 | 0 | oxide | 0 | 5 |
| petstore-31 (fixture) | 1 | 1 | posthog | 5 | 6 |
| qdrant | 4 | 4 | superset | 5 | 10 |
| telnyx | 7 | 10 | twitter | 0 | 2 |
| weather-gov | 0 | 8 | xai | 0 | 2 |
| zendesk | 0 | 1 |  |  |  |

Jellyfin supplies 381 of the 596 published arms and is the dominant download API. The audit expected
GitHub to dominate beside it, but the generated-arm census finds only four GitHub arms; Cloudflare
is the second-largest document at 44. This distinction matters because the count is selected arms,
not every alternate media type a response lists.

Correct buffered handling is the shipped boundary: text becomes `String`, binary/unknown becomes
the configured byte representation, and both are read from and written to the raw HTTP body under
the declared content type. A raw streaming response handle remains follow-up work, measured against
a large-body fixture before adoption; buffered correctness does not claim streaming parity.

The upload-side census confirms that its analogous follow-up is real but separate: **41 raw-byte
request bodies and 89 multipart bodies carrying 108 file parts, across 32 published documents**.
Cloudflare contributes 12 raw bodies plus 15 multipart bodies, OpenSearch 16 raw bodies, and the
remaining shapes are distributed rather than concentrated in one API. Request streaming is
therefore retained as measured follow-up work, to be designed with the raw download handle rather
than folded into the response correctness repair without a large-body runtime gate.

## Value constraints: stated, not enforced

The parser already preserved JSON Schema value constraints, but classification used to drop them
without a diagnostic. The corpus contains **39,074 active constraint occurrences** across the 78
documents. Counts below are parsed keyword occurrences after normalization; document incidence is
the number of descriptions carrying at least one. `uniqueItems` counts only `true`, because
`uniqueItems: false` imposes no constraint.

| keyword | occurrences | documents |
| --- | ---: | ---: |
| `multipleOf` | 39 | 7 |
| `maximum` | 5,728 | 65 |
| `exclusiveMaximum` | 10 | 4 |
| `minimum` | 8,209 | 70 |
| `exclusiveMinimum` | 259 | 20 |
| `maxLength` | 10,220 | 54 |
| `minLength` | 5,780 | 52 |
| `pattern` | 5,125 | 46 |
| `maxItems` | 1,547 | 48 |
| `minItems` | 1,257 | 50 |
| `uniqueItems` | 664 | 18 |
| `maxContains` | 0 | 0 |
| `minContains` | 0 | 0 |
| `maxProperties` | 135 | 12 |
| `minProperties` | 101 | 12 |

Equal positive `minItems`/`maxItems` bounds at or below the fixed-array limit are already carried by
the generated `[T; N]`; **72 generated shapes** take that path. Every other active keyword enters
the uninterpreted-constraint diagnostic, folded by its keyword set with an occurrence count and a
bounded sample of locations. Value constraints beside `$ref` participate in the shape key so they
cannot disappear with the transparent reference.

`uniqueItems` deliberately stays a `Vec`. The 664 active declarations make the constraint real, but
a set deserializer would silently discard duplicate wire elements and may reorder what arrived.
Preserving every element in arrival order is the faithful data model; the diagnostic states that
uniqueness is not enforced.

No `constraints = "enforce"` mode is added. The 39,074 occurrences are not one feature: they span
numeric, string, array, object, and `contains` semantics across almost the entire corpus. An honest
mode would be a second generated validation type system with constructor and deserialization policy
for every constrained position; a regex-only string newtype would make the broad configuration
name promise more than it enforces. The default and only mode is therefore explicit: preserve,
diagnose, and do not enforce. Validation remains application policy until consumer demand can
justify that API surface, its runtime dependencies, and a disciplined full-surface compile A/B.

## Connection upgrades and IP formats

The status census finds **24 operations declaring exact `101 Switching Protocols` responses across
4 of 78 documents**: Cloudflare 19, LangSmith 2, Oxide 1, and Telnyx 2. This overturns the audit's
expectation that Oxide would dominate. Every exact arm now emits a `connection-upgrade` degradation
stating that the generated arm carries only the status: the client does not return upgraded I/O and
the server cannot serve it.

Full upgrade support is not generated from that status alone. The four descriptions identify an
HTTP transition but do not share a contract for subprotocols or frames, so a typed feature would
invent the protocol it claims to derive. A raw upgraded connection remains part of the measured raw
streaming transport follow-up, where a full-duplex fixture can define behavior without pretending a
`101` response schema describes WebSocket frames.

The string-format census finds **41 `ip` occurrences in 2 documents, 31 `ipv4` occurrences in 4,
and 11 `ipv6` occurrences in 3**. That is enough incidence for a dependency-free fidelity gain:
the formats now lower to `std::net::IpAddr`, `Ipv4Addr`, and `Ipv6Addr`, and the generated probe
synthesizes valid addresses for each. `uri` and `hostname` remain strings; this census did not ask
for a new validation dependency.

## Small surface decisions

- The plan reports configurable `Default` as absent, but the cited `Derive` enum already contains
  it at the audited commit. Its fixed-point eligibility also already excludes enums and propagates
  only through members that can default. A direct test now pins that existing behavior; `Default`
  stays outside the default derive set.
- `formats.bytes` controls raw binary request and response bodies. A base64 or binary property
  inside JSON stays `String`, which is the text the wire actually carries; turning it into bytes
  would silently choose a codec. The renderer no longer matches the byte knob only to return
  `String` from both arms, and the public configuration docs state the boundary.
- The compile-cost headline beside `SerdeImpl` now gives both scopes: 65–67% less CPU on the
  focused type layer, and 31–44% less CPU plus 22–37% less peak RSS over generated types, client,
  and server.
- Generated clients keep no hook system. A configured `reqwest::Client` supplies transport-wide
  authentication, retry, and tracing behavior; what this deliberately loses is automatic
  per-operation identity. README records the escape for applications that need it: wrap generated
  operation methods in an application-owned client that creates named spans.
- The probe's recorder double remains harness infrastructure rather than a new `emit.double`
  surface. Its renderer synthesizes responses and stores values in a probe-private assertion and
  lifecycle shape. Shipping that would commit public storage, assertion, and server-lifetime APIs,
  not merely package reusable code. No consumer demand presently justifies that product surface;
  the real-socket probe remains its behavioral prototype.

## Workspace packaging for large descriptions

`packaging = "workspace"` is an opt-in third shape beside the default single crate and the
build-script module. It emits `<name>-types`, `<name>-client`, and `<name>-server`; the types member
has no features, and each edge pins its exact generated types version by path. This is the
load-bearing boundary: Cargo cannot unify an HTTP feature into a crate that has no such feature, so
a domain crate can depend on the wire model without inheriting reqwest, axum, or their generated
source. The generated README records the dependency-order release procedure: publish types first,
then client and server.

The private support runtime follows the consumer rather than leaking features back into types.
Buffered serde and shared value helpers stay in types, client HTTP helpers live in client, and
router/response helpers live in server. All three crates deny warnings. The corpus compile harness
therefore compiles generated workspaces, not a single crate with a different feature combination;
the quick tier passes through both serde strategies, including Cloudflare through the derive.

`bench-compile` treats a workspace as one benchmark unit whose members always run in dependency
order. Each member is selected and cleaned exactly, with incremental compilation disabled, so a
client sample cannot accidentally reuse a server build or charge a freshly invalidated types crate.
Independent variants still alternate A-B-B-A. The durable record carries CPU, direct elapsed wall
time, peak RSS, load, pressure, and scope per member; it refuses fewer than three kept repetitions,
load above 5, or memory pressure. Wall time is measured rather than inferred from RSS because the
packaging question adds three sequential Cargo invocations, and memory size says nothing reliable
about their startup or metadata-loading cost.

The derive Workspace take records every quick-tier member directly:

| document | sequential wall | worst member RSS | sum of member peaks |
| --- | ---: | ---: | ---: |
| Petstore | 0.65 s | 161.4 MiB | 0.40 GiB |
| GitHub | 93.19 s | 3.16 GiB | 9.46 GiB |
| PostHog | 121.31 s | 3.68 GiB | 11.03 GiB |
| Cloudflare | 327.22 s | 9.18 GiB | 27.36 GiB |
| Okta | 59.24 s | 2.05 GiB | 6.15 GiB |
| Orb | 26.79 s | 1.12 GiB | 3.35 GiB |
| Jellyfin | 18.23 s | 676.6 MiB | 1.98 GiB |
| Oxide | 17.16 s | 666.5 MiB | 1.95 GiB |

That is **663.79 seconds of direct sequential wall across 24 invocations**. Cloudflare's six kept
repetitions put its largest member at 9.18 GiB mean and 9.19 GiB maximum observed, against the old
single-crate derive take's pressured 14.7 GiB floor. It now fits below the roughly 14.5 GiB
available on a 16 GB hosted runner with room for the process around rustc, so the derive CI
exclusion retires. The reduction is at least 37%; the old side is only a floor, so claiming a more
precise percentage would manufacture precision from memory pressure.

The hand-written packaging A/B measures the wall trade directly on identical renderings:

| document | crate → Workspace wall | wall delta | crate → worst member RSS | RSS delta |
| --- | ---: | ---: | ---: | ---: |
| Petstore | 0.33 → 0.70 s | +116.0% | 187.3 → 161.8 MiB | −13.6% |
| GitHub | 22.45 → 36.40 s | +62.1% | 3.27 → 1.31 GiB | −60.0% |
| PostHog | 34.56 → 50.64 s | +46.6% | 4.55 → 1.67 GiB | −63.2% |
| Cloudflare | 69.99 → 116.97 s | +67.1% | 8.82 → 3.70 GiB | −58.0% |
| Okta | 14.22 → 23.83 s | +67.6% | 2.17 GiB → 926.6 MiB | −58.2% |
| Orb | 4.82 → 9.48 s | +96.8% | 930.3 → 523.9 MiB | −43.7% |
| Jellyfin | 6.82 → 9.25 s | +35.8% | 1.17 GiB → 593.7 MiB | −50.6% |
| Oxide | 5.10 → 7.85 s | +53.8% | 908.6 → 449.9 MiB | −50.5% |

Across the tier, direct sequential wall is **158.27 → 255.14 seconds (+61.2%)**. This is why
Workspace remains opt-in: it buys a 44–63% worst-unit RSS reduction on every non-trivial tier
document and a real dependency boundary, while paying for three Cargo/rustc invocations. The
Cloudflare result reproduces the original hand-split experiment (9.10 → 3.84 GiB) on the product
renderer at 8.82 → 3.70 GiB.

The shared host also found two holes in the recorder rather than in the packaging. A Cloudflare
server sample consumed 279 seconds after two at 108/122 while satisfying load-at-start and
processor-progress checks: memory-bandwidth contention can begin mid-sample and inflate CPU and
wall together. Recording now persists the cheapest-to-most-expensive CPU range and refuses a range
exceeding both 25% and 3 seconds. Both limits matter: Orb's 8.84/9.00/11.33-second diagnostic take
was 28% but only 2.49 scheduler-scale seconds. A memory-pressured Cloudflare attempt then proved
that pressure must discard an attempt rather than keep it and poison otherwise valid repetitions.
Both real sample sets are red/green tests; the recorded means contain no pressure-disqualified
attempt.

## Runtime cost of the buffered serde default

The compile-time default now has a runtime budget as well as a compile-cost measurement.
`xtask bench-runtime` generated types-only crates for both serde strategies, compiled them outside
the samples, and deserialized the quick tier's complete named payload set in A-B-B-A order. Four of
the eight documents carry 643 payloads: 608 ordinary examples and 35 examples the description
itself contradicts; another 171 payload positions cannot be checked. The benchmark adds one
277,664-byte body with 2,048 large fields before a depth-24 malformed tail, so the result does not
quietly extrapolate from short objects.

Four repetitions per strategy were kept, none discarded. The worst starting load was 4.96, below
the enforced ceiling of 5, and neither strategy ran under memory pressure. Each value below is one
complete pass over the tier payloads plus the synthetic fixture; compilation and process startup
are outside the timed region. Peak heap is the counting allocator's maximum logical live heap, not
process RSS.

| strategy | valid wall | valid allocations | valid peak heap | malformed wall | malformed allocations | malformed peak heap |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| derive | 0.713 ms | 12,380 | 496.0 KiB | 0.470 ms | 8,425 | 496.1 KiB |
| hand-written | 2.315 ms | 23,414 | 1.68 MiB | 1.744 ms | 17,035 | 1.68 MiB |
| hand-written / derive | **3.25×** | **1.89×** | **3.47×** | **3.71×** | **2.02×** | **3.46×** |

The published budget is hand-written at no more than **4.5× derive wall time, 2.25× allocations,
and 4× peak heap** on either path. Both strategies rejected 35 of the 36 malformed cases and
accepted the same one; all 36 outcomes have the same message. The deep tail is rejected by both.
Trailing serde line/column offsets differ after buffered replay and remain recorded rather than
compared, matching the differential harness's reviewed exception: buffering cannot recover an
input offset after the format has read beyond that member.

These are deliberately types-only runtime numbers. They do not claim to measure client/server
transport time, and no types-only figure is quoted as a full generated-surface result. The
machine-readable workload, raw means, exact offsets, discipline evidence, ratios, and refusal
thresholds are committed in [`runtime.toml`](runtime.toml).

## The monomorphic assemble experiment, measured and reverted

The fixed-error experiment did reduce the generic source surface it targeted. The body census
differences nightly expanded output at one and eleven otherwise-identical generated structs:

| rendering | total bodies/type | serde bodies/type | generic bodies/type |
| --- | ---: | ---: | ---: |
| derive | 12 | 10 | 8 |
| hand-written, generic `assemble<E>` | 5 | 3 | 3 |
| hand-written, fixed assembly error | 5 | 3 | 2 |

On the deserialization side, that is the predicted two generic bodies becoming one; `serialize<S>`
remains the other generic serde body. The total body count does not change. All 16 differential
cases preserve their exact error messages under the fixed error and again after its removal.

The compile A/B used archived and experimental renderings of every quick-tier document. Recursive
source comparison finds only `support.rs` and `types.rs` changed in all eight generated crates, and
every inspected hunk is the fixed error or `assemble` signature. Each subject includes generated
types, client, and server in one crate, with incremental compilation disabled and one compiler job.
Variants alternate A-B-B-A; every recorded side has at least three kept repetitions, starts at load
at most 5, has memory headroom, and satisfies both the per-attempt crowding rule and the recorded CPU
spread ceiling.

| document | generic → fixed CPU | CPU delta | generic → fixed peak RSS | RSS delta |
| --- | ---: | ---: | ---: | ---: |
| Petstore | 0.296 → 0.303 s | +2.53% | 0.185 → 0.183 GiB | −1.07% |
| GitHub | 26.070 → 24.300 s | −6.79% | 3.270 → 3.268 GiB | −0.09% |
| PostHog | 34.890 → 34.490 s | −1.15% | 4.556 → 4.552 GiB | −0.09% |
| Cloudflare | 74.788 → 74.104 s | −0.91% | 8.844 → 8.823 GiB | −0.24% |
| Okta | 14.312 → 14.202 s | −0.77% | 2.178 → 2.166 GiB | −0.56% |
| Orb | 4.919 → 4.756 s | −3.32% | 0.907 → 0.907 GiB | −0.01% |
| Jellyfin | 6.763 → 6.784 s | +0.30% | 1.175 → 1.173 GiB | −0.19% |
| Oxide | 5.142 → 5.085 s | −1.11% | 0.885 → 0.886 GiB | +0.13% |

Tier CPU is **167.18 → 164.02 seconds (−1.89%)** and direct wall is
**167.50 → 164.17 seconds (−1.99%)**. GitHub carries the only result above ordinary low-single-digit
movement, and its generic side also carries the tier's largest accepted CPU spread at 20.7%;
Cloudflare, the largest and most decisive subject, moves less than 1% CPU and 0.25% RSS. The
direction is not even uniform on CPU, and no document has a material RSS change.

The experiment is therefore **not adopted**. Its extra error type and conversion boundary buy no
measurable compile-memory improvement and only a sub-2% aggregate timing movement, so the generic
implementation was restored. Item 7's runtime record remains the shipping baseline: reverting the
experiment means no new allocation or malformed-path behavior enters the product. The exact body
counts, raw means, repetition/discard counts, loads, spreads, and decision are durable in
[`assemble.toml`](assemble.toml).

## The architecture review, and what reading found that running had not

A full review of the crate and its harness, after the probe work: verdict *sound shape* — the
pipeline fits the domain and no structural move was recommended — and a list of defects reading
found that no gate had, because each sat exactly where the gates do not look.

- **Generated manifests missed dependencies.** The dependency walk covered named contracts only,
  so a `format: uuid` appearing only as a parameter, or a `map` only in a response arm, rendered
  `uuid::Uuid`/`indexmap` into a crate whose manifest never declared them — and `::bytes::Bytes`,
  spelled by the client for a binary body under `formats.bytes = "bytes"`, had **no manifest line
  at all**. The walk now covers the API surface, and the manifest test pins where each crate is
  named. Found by asking what the manifest was computed *from*, which no tier document happened to
  exercise.
- **Three answers to "which arm is success".** Pagination open-coded `Exact(2xx)` and rejected a
  document declaring only `2XX` — an arm the client happily decodes. The `weather-gov` defect of
  stage 5, unfixed in two more copies; `StatusPattern::is_success` is the one rule now.
- **`readOnly`/`writeOnly` were computed and dropped**, with no diagnostic — the forbidden
  silence, in a crate whose charter is that every deviation is reported. They now ride the same
  machinery as the presence collapse and report as `access-collapse`, only where the type actually
  crosses the direction the marker excludes: 23 records across the corpus, `zendesk` alone
  carrying 211 `readOnly` members into request bodies.
- **A tuple's permissive tail was dropped silently.** `prefixItems` beside an `items` that admits
  values allows instances longer than the prefix; the generated `(A, B)` refused them and nothing
  said so. Degraded loudly now — and `items: false`, which is draft-04's `additionalItems: false`
  normalized, is read as the fixed tuple it spells, which *removed* a misleading
  "accepts no value" record from `meilisearch`.
- **The presence split misattributed form-reached types.** Reachability seeded from JSON bodies
  only, so a type reached through a multipart or form body — 278 multipart bodies across 27
  documents — was reported as "no operation's body, nothing on the wire is affected". A false
  statement about the wire, in the diagnostic the split exists to make true.
- **An exploded `form` object parameter is erased by its own encoding**, and the generated
  handler's member is permanently absent — required, and the handler rejects every request. It
  now reports under `query-serialization-style` (9 records; `workos` writes two), and the probe
  records a named skip instead of a red test.
- **`emit.types = false` silently generated nothing**: an empty `files` map with a success code —
  the silent no-op, in progeny's own configuration. Refused now.
- **One `$schema` produced two `unsupported-dialect` records**, one from the normalizer's walk
  and one from the schema parser, differently worded. The parser — the layer that stores the
  member — is the one reporter, and the two hand-copied dialect lists are one.

The other half of the review was drift that had not yet fired, now held mechanically: the layer
lint's table is **total** (an unranked module is itself a violation — `resolve`, a whole pipeline
stage, had been silently exempt in both directions since the day it was added); the eight-method
path-item walk exists once instead of seven times, and `Method` lives beside the fields it
mirrors so no mapping needs a fallback arm; the document parser and serializer are pinned
member-by-member by a maximal-document mirror, the way `EVERY_KEYWORD` already pinned the schema
layer's; the normalizer's subschema lists are compared outright against the model's applicators;
every `support::…` path the renderers spell is resolved against the shipped module's actual
items; the two `Style` enums are bridged by an exhaustive match instead of a string table, and
the shipped table's wildcard arms are spelled out so a new style cannot silently encode as
`form`; `BreakageClass::ALL` is grounded in serde's own variant list and the catalogue iterates
it instead of parsing an error message; and the harness gates share one document-selection with
one refusal — the payloads gate could previously exit green having checked zero documents when
the cache was missing.

## The review's structural tail: one value rule, and a tag the types carry

The two items the review filed as design tasks rather than patches, implemented after it — each
re-measured over the corpus.

**A tagged variant without its wire bytes is now unconstructible.** "`tag_value` is set exactly
when the tagging is `Internal`" was asserted in four comments and enforced by none; a variant that
broke it would have had serde write the *Rust* variant name onto the wire. The tagging enum and its
`Option<String>` are gone — an internally-tagged union is its own contract kind, and every one of
its variants carries its tag bytes as a plain `String`. Making the pairing structural surfaced a
second hole the review had not named: the dedup fingerprint compared variants by Rust name and
type, and a Rust name is a *normalization* of the tag value — `a-cat` and `aCat` both name an
`ACat` — so two unions that differ only in the bytes their tags read could merge, handing one
document position the other's wire format. The corpus answered that no document contains the
colliding pair (78/78 snapshots byte-identical), so the fix ships as a guard with a test that was
verified to catch the merge, not as a repair to any output.

**One rule for a JSON value against a schema.** A declared `default` was checked shallowly against
the lowered *type*, and an `example` recursively against the *shape* — two checkers that could
disagree about the same literal, and over the corpus did, in both directions. The rule now lives
below both layers (`shape::Fit`) and both verdicts come from it; re-running the corpus re-ruled 14
documents, 105 recorded invalid-default occurrences becoming 67:

- **50 valid defaults were being dropped.** The shallow checker's `Named` arm accepted only
  objects and strings, so any scalar default behind a reference was refused: `cloudflare` alone
  gets 30 back — `build_caching_enabled`'s `true` behind an `allOf` to a boolean component among
  them — beside `logfire`'s `false` on a boolean-or-`"allow-local"` union (8) and `gladia`'s `16`
  on a number enum that lists it (4). All now kept, and each generated field's documentation
  gained its "the server assumes …" line back.
- **12 genuinely impossible defaults were passing silently.** The same arm asked nothing about an
  object's members or an enum's values: `meilisearch` defaults `mode` to `"Human"` and its own
  enum allows `human`, `json`, `profile`; `oxide`'s allocator defaults satisfy no branch of their own
  unions; five `cloudflare` ruleset defaults omit members their schemas require. Each is now
  dropped with the contradiction spelled out, because serde could never have deserialized it.
- The rest re-keyed in place: `okta`'s 18 split into three reasons with the same total, and every
  surviving record now says *why* — "the schema says an integer, the default is a string" — in
  the same vocabulary the example checker has always used, because it is now the same sentence
  from the same rule.

The corpus also pinned the disagreement shut end-to-end: one wrong literal written as both a
property's `default` and the payload's `example` produces both records, same reason, each in its
own words.

## The operations reflection, measured

`operations` is a fourth module beside `types`, `client` and `server`: every operation the model
kept as an exhaustive enum, the registrable subset as a second one, and one `static` table of
`rust_name`, method and template per operation, rendered from the same finalized model as the other
two and emitted whenever there is something to call. It has no dependency and no flag, so the
question it had to answer before landing was what it costs the crate that carries it — the types
member of a Workspace, which is also the crate a types-only consumer pays for.

Measured as an archived A/B: the quick tier rendered by the tree before the module and by the tree
with it, both under Workspace packaging and the hand-written serde default, the before side renamed
so both could sit in one plan and alternate A-B-B-A within each repetition. Three repetitions on an
11-core Apple M-series laptop at `--max-load 6`; two of 144 repetitions were discarded for external
load (a browser and Spotlight, neither the benchmark's), and posthog was re-measured alone after its
first take read two before-side server peaks 0.3 GiB apart for identical source. The machine reports
no available-memory figure, so the pressure check was off throughout.

| document | types CPU | types peak RSS | client CPU | server CPU |
| --- | ---: | ---: | ---: | ---: |
| petstore-31 | 0.14 → 0.15 s (+7.1%) | 119.8 → 121.6 MiB (+1.5%) | +2.9% | +6.9% |
| github-31 | 9.04 → 8.77 s (−3.0%) | 1.58 → 1.70 GiB (+7.6%) | −5.8% | −5.4% |
| posthog | 13.12 → 13.58 s (+3.5%) | 2.42 → 2.36 GiB (−2.5%) | +0.8% | +1.9% |
| cloudflare | 24.17 → 24.86 s (+2.9%) | 3.25 → 3.27 GiB (+0.6%) | +1.1% | +1.5% |
| okta | 4.78 → 4.71 s (−1.5%) | 1010.5 MiB → 1.01 GiB (+2.3%) | +1.1% | +1.3% |
| orb | 2.56 → 2.57 s (+0.4%) | 564.1 → 567.9 MiB (+0.7%) | +2.5% | +0.6% |
| jellyfin | 1.52 → 1.57 s (+3.3%) | 384.4 → 398.0 MiB (+3.5%) | +0.4% | +1.1% |
| oxide | 1.32 → 1.44 s (+9.1%) | 370.4 → 383.2 MiB (+3.5%) | +2.9% | −4.2% |

The bound the design set — no member past the 10% regression threshold, and the `cloudflare` and
`github-31` types members within 3% CPU — holds. Cloudflare's types member pays +2.9% CPU for 3,200
rows and github's reads −3.0%; posthog's, with 8,198 rows the largest table in the tier, +3.5%. The
largest CPU movement anywhere is oxide's types member at +9.1%, which is 0.12 s of a 1.3 s compile,
below the 3-second floor the recorder itself uses to tell a material spread from scheduler jitter.
Peak RSS on the types member moves by −2.5% to +7.6%, the table and the two enums being one more
item rustc holds; the client and server members are within noise, because neither reads the table
— the router only spells its templates through `Route::X.path()`. So the module stays always-on,
and the `Emit::operations` fallback the design held in reserve is not taken.

Two things the run said about the recorder rather than the module. The scope string now includes
the reflection (`types+operations`, and `types+operations+client+server` for the one-crate
control), so the checked-in baseline's entries of the old shape — every types member and every
crate control, 40 of 72 — are refused as a comparison basis until the baseline is re-recorded on
the reference machine: `task bench:compile -- --ab --workspace --crate-control --generate-only` at
the tree under review, then `task bench:compile -- --ab --reuse --reps 6 --max-load 5
--write-baseline` when it is idle, which is the README's release step. The refusal now names that
remedy, because a bail that only refuses reads as the harness being broken rather than the
baseline being stale. And macOS has no `/proc/loadavg`, which the load gate read, so no
repetition could start there until it asked `sysctl` instead; available memory stays unknown on
macOS, and a peak taken under pressure there reads low without being flagged, which is what the
first posthog take showed.

## Diagnostics the corpus produces

Every finding, per document, is recorded in `corpus/snapshots/*.jsonl`, keyed by the SHA-256 of the
document it was taken from. That key is what makes snapshots workable for documents that are fetched
rather than committed: a mismatch with a *changed* hash is "upstream republished, re-baseline", and a
mismatch with an *unchanged* hash is "we regressed". Without the split, a vendor's routine
republication is indistinguishable from a bug.

Aggregation is what keeps the suite readable: a class that fires at scale — 642 tuple rewrites in one
document, 19 name collisions in another — is one record with a count and the first five locations
rather than 642 lines nobody reads.

**Stage 5 tested that rule and had to apply it again.** Operations arrived and
`colliding-operation-id` produced **1,058 records — 1,052 of them the same finding**: an operation
that declares no `operationId` and is therefore named after its method and path. Six were genuine
collisions. A per-occurrence class at that scale is the failure mode the aggregation rule exists to
prevent, so the class now aggregates and the missing-id sentence deliberately names no identifier —
records fold on their sentence, so the 1,052 become one per document with a count, while a real
collision names both identifiers and stays its own record. The names are not lost; they are in the
generated source, which is where a reader would look for them anyway.

## Drift found in the manifest

`hetzner-cloud` was recorded as 3.0 and now serves 3.1; the manifest was corrected. The corpus
runner cross-checks every document's declared version against the manifest on every run, so this
kind of rot surfaces rather than accumulating.
