---
title: Diagnostics
weight: 4
---

# Diagnostics

Published API descriptions are broken in recurring, classifiable ways, so tolerance in progeny is a designed feature rather than accumulated patches: a closed taxonomy of actions, a closed catalogue of breakage classes, and a record for every deviation from the input document.

## A record

Each diagnostic names what progeny did (its **action**), which way the document was broken (its **breakage class**), where (a JSON Pointer into the document), and a sentence saying what was found and what was done about it. The CLI prints one per line:

```text
degrade: unsupported-construct at /components/schemas/FusionConnectionBackfillRequest/properties/streams: `minItems` narrows which values are valid in a way no Rust type expresses; the generated type accepts more than the document allows (2 occurrences) (see also /components/schemas/RowFilteringOperationNot/properties/conditions)
```

The same record as one JSON line is what the [corpus snapshots]({{< relref "corpus.md#diagnostics-snapshots" >}}) check in:

```json
{"class":"unsupported-construct","action":"degrade","location":"/components/schemas/FusionConnectionBackfillRequest/properties/streams","detail":"`minItems` narrows which values are valid in a way no Rust type expresses; the generated type accepts more than the document allows","occurrences":2,"related":["/components/schemas/RowFilteringOperationNot/properties/conditions"]}
```

A class that fires at scale is aggregated: one record per document carries the occurrence count, the first location, and the next few. A class whose every occurrence needs its own decision, such as a discriminator edge case, keeps one record per occurrence.

## Actions

| Action | Meaning |
| --- | --- |
| `repair` | A well-understood fix with confident semantics; the output is faithful to the document's evident intent. |
| `degrade` | Represented less precisely than the document asked, but soundly: [outward, never inward]({{< relref "generated-code.md#soundness-degrade-outward-never-inward" >}}). |
| `warn` | Generation unaffected; the document has a smell worth surfacing. |

Rejection is not an action. A document progeny cannot use at all is an error with no output, and there is no partial rejection: anything short of it produces output plus diagnostics. The reasons are `unparsable`, `non-scalar-key`, `not-an-object`, `missing-version`, `unsupported-version`, `no-operations`, and `unsatisfiable-config` — a configuration that asks, by name, for something the document cannot be given.

## Breakage classes

The catalogue is closed, with no catch-all: a newly observed way for descriptions to be broken has to become a class of its own.

| Class | What it reports |
| --- | --- |
| `malformed-member` | A member progeny interprets held a value of the wrong shape; it is preserved verbatim and not read. |
| `missing-final-line-break` | A YAML document ending inside a block scalar without a line break. |
| `non-finite-number` | A number the model cannot hold: YAML `.inf`/`.nan`, or one beyond an `f64` without `exact-numbers`. |
| `inexact-number` | A number held only as its nearest value, without `exact-numbers`. |
| `unsupported-dialect` | A `$schema` naming a dialect progeny does not implement; interpreted as 2020-12. |
| `dynamic-scoping` | `$dynamicRef`/`$dynamicAnchor`, resolved as the plain `$ref`/`$anchor` form. |
| `dangling-ref` | A `$ref` that resolves to nothing, after the best-effort repairs. |
| `unknown-schema-type` | A `type` value that is not one of the seven JSON Schema types. |
| `multi-parent-discriminator` | A schema named by the discriminator mappings of more than one parent. |
| `discriminator-edge-case` | A discriminator whose mappings are incomplete or name variants implicitly. |
| `inheritance-discriminator` | A discriminator and mapping on a schema with no `oneOf`/`anyOf`, read as the union its mapping names. |
| `wild-union` | A union whose "any combination may match" semantics has no faithful Rust type. |
| `colliding-operation-id` | An operation without an `operationId`, or one that sanitizes to a name another operation has. |
| `connection-upgrade` | A switching-protocols response the generated surfaces do not hand over. |
| `query-serialization-style` | A parameter style combination OpenAPI leaves undefined. |
| `invalid-example` | An example payload that contradicts its own schema. |
| `invalid-default` | A `default` that does not typecheck against its own schema. |
| `unregistrable-route` | A route the generated router could not register, a collision, or a template and parameters that disagree. |
| `legacy-tuple-items` | The draft-04 tuple form `items: [A, B]`. |
| `legacy-ref-siblings` | Keywords beside a `$ref` in a draft-07 or earlier bundle, which that draft ignores. |
| `legacy-dependencies` | draft-07's `dependencies`. |
| `legacy-exclusive-bound` | The 3.0 boolean `exclusiveMinimum`/`exclusiveMaximum` in a 3.1 document. |
| `legacy-string-format` | The 3.0 `format: byte`/`binary` spelling in a 3.1 document. |
| `legacy-required-flag` | The draft-03 `required: true` flag on a property. |
| `nullable-union-branch` | A 3.0 union branch whose only content is `nullable: true`. |
| `multi-media-type` | A position declaring several media types, of which one is primary. |
| `unsupported-construct` | A construct progeny does not interpret — `not`, `if`/`then`/`else`, `dependentSchemas`, `unevaluated*`, a value constraint such as `pattern` — so the type accepts more than the document allows. |
| `irreconcilable-all-of` | An `allOf` whose branches cannot be merged into one shape. |
| `presence-collapse` | An optional and nullable property collapsed onto one `Option`. |
| `access-collapse` | A `readOnly` or `writeOnly` property in a type that crosses the direction the marker excludes. |
| `encoding-collapse` | A schema sent as `multipart/form-data` and also used at another wire position. |
| `colliding-type-name` | Two shapes whose names sanitize to the same Rust identifier. |
| `unsatisfiable-derive` | A derive asked of every type, on a type that cannot have it. |
| `nonproductive-cycle` | Schemas whose lowered types are aliases and wrappers of one another with no member, element or `null` between them, such as two single-branch `oneOf`s naming each other; no finite value is one, so each is typed as arbitrary JSON. |

## Denying diagnostics

Strictness is caller policy rather than library judgement: progeny always returns every diagnostic, and the caller — a build script, a binary, CI — decides which ones stop the build. `[deny]` in `progeny.toml` says so by action, by class, or both:

```toml
[deny]
actions = ["degrade"]
classes = ["dangling-ref"]
```

A vendored document with a few known findings can still fail the build on the next one. Each reviewed finding is accepted by its class, action and exact location:

```toml
[[deny.accept]]
class = "unsupported-construct"
action = "degrade"
at = "/components/schemas/Thing/properties/p0"
reason = "the vendor's `not` is advisory"
```

An accepted finding is still reported, marked as accepted, and never denied. A new occurrence elsewhere, even inside the same schema, needs its own review. An entry that no longer matches anything is refused too, because it would let the same finding back in, unreviewed, if it ever returned.

The CLI exits non-zero when a diagnostic is denied or an accepted entry no longer occurs. A library caller asks `Config::denied` for the denied records and reads `Output::unmatched_accepts`.
