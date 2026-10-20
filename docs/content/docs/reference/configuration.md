---
title: Configuration
weight: 2
---

# Configuration

`progeny.toml` is everything a caller can say, as a closed set of typed values. It is strict: an unknown key or value is a parse error, and a setting the document cannot honour stops generation rather than being ignored. Every top-level key is optional, and an empty file is the default configuration; a table that is present may still require some of its own keys, as noted below.

The CLI reads it with `--config`. A library caller deserializes the same TOML into `progeny::Config`, or builds one in Rust.

## Addressing the document

Every key that names something in the document uses one grammar. A key starting with `/` is a JSON Pointer to where the thing is written, such as `/components/schemas/Pet` or `/paths/~1pets/get`. Anything else is a name in the thing's own namespace: the `components.schemas` name for a type, the generated method name for an operation. A key that names nothing is an error.

## Output

### `packaging`

Whether to emit a crate, a workspace, or a module. See [packaging]({{< relref "../generated-code.md#packaging" >}}).

| Value | Output |
| --- | --- |
| `"crate"` | A complete crate: a manifest and a `src` tree. The default. |
| `"workspace"` | `<name>-types`, `<name>-client` and `<name>-server` crates. |
| `"module"` | One file, `progeny.rs`, to `include!`. |

### `package`

What to call the emitted crate. Without the table, the crate is `api` at version `0.1.0`. Once the table is present, `name` and `version` are both required. A name that is not a legal crate name or a version Cargo would refuse stops generation.

```toml
[package]
name = "petstore"
version = "1.2.0"
```

### `emit`

Which halves of the interface to emit. `types`, `client` and `server` all default to `true`. The client and the server both build on the type layer, so `types = false` is refused.

```toml
[emit]
server = false
```

### `docs`

Whether the document's prose becomes doc comments. `"schema"`, the default, keeps the `title`, `summary` and `description` the document writes. `"none"` leaves them out, for a schema exported from Rust whose intra-doc links name types the generated crate does not have. `#[deprecated]` and the doc comments progeny writes itself are emitted either way.

## Serde and decoding

### `serde-impl`

Which `Serialize`/`Deserialize` implementation strategy to use.

| Value | Meaning |
| --- | --- |
| `"hand-written-where-eligible"` | The default. Hand-written implementations wherever eligible; structs need a self-describing format. |
| `"derive-always"` | The serde derive on every type, for formats such as `bincode` or `postcard` that do not name their members. |

### `decoding`

How a generated client reads responses. `"lenient"`, the default, decodes as far as the payload allows into read forms and reports what did not match. `"strict"` decodes into the strict types and fails the response on a deviation such as a missing required member or a value of the wrong type; it leaves out `types::read`, so the response types change with it. String enums and unions stay open under either setting, keeping a value the description does not list in their fallback variant, and undeclared members follow [`unknown-fields`](#unknown-fields-and-type-unknown-fields). The CLI's `--decoding` overrides this key. See [reading responses]({{< relref "../generated-code.md#reading-responses" >}}).

### `unknown-fields` and `type-unknown-fields`

What to do with members a payload has and the document did not declare, under strict decoding: the request bodies a generated server reads, the strict form of every type, and so every response under `decoding = "strict"`. Under lenient decoding a read form with a twin of its own keeps undeclared members in `extra`; a type shared by requests and responses reports them and does not keep them, because the same type is what a request sends.

| Value | Meaning |
| --- | --- |
| `"ignore"` | Accept and discard them. The default. |
| `"deny"` | Refuse the payload. |
| `"capture"` | Keep them in a map on the type. |

`type-unknown-fields` sets the same per type:

```toml
unknown-fields = "ignore"

[type-unknown-fields]
CreatePetRequest = "deny"
```

## Types

### `formats`

Which crates the generated types use for the formats that have a choice. The defaults are the dependency-free ones.

| Key | Values |
| --- | --- |
| `date-time` | `"string"` (default), `"chrono"`, `"time"`, `"jiff"` — for `date-time`, `date` and `time` |
| `uuid` | `"string"` (default), `"uuid"` |
| `bytes` | `"vec"` (default), `"bytes"` — for raw binary bodies; base64 inside JSON stays `String` |
| `integers` | `"wide"` (default): `i64`, or `u64` where a `minimum` of zero rules out a sign. `"by-format"`: the width and sign the `format` names, `int8` to `uint64` |

```toml
[formats]
date-time = "jiff"
uuid = "uuid"
```

### `map`

Which map type generated code uses: `"b-tree-map"` (the default, for deterministic iteration), `"hash-map"`, or `"index-map"`, which preserves the order members arrived in.

### `derives` and `type-derives`

Derives to add to generated types, from a closed set: `"clone"`, `"debug"`, `"partial-eq"`, `"eq"`, `"hash"`, `"partial-ord"`, `"ord"`, `"default"`, `"copy"`. `derives` applies to every type eligible for them, and an ineligible type is reported as `unsatisfiable-derive`. `type-derives` names types, and an ineligible request there is an error.

```toml
derives = ["clone", "debug", "partial-eq"]

[type-derives]
Pet = ["eq", "hash"]
```

### `names`

Names to use instead of the ones progeny derives, keyed by component name or schema pointer. An explicit name is a declaration of identity: two shapes named here never merge, even when they are structurally identical.

```toml
[names]
"/components/schemas/Pet/properties/owner" = "PetOwner"
```

### `preserve-optional-nullable`

Optional and nullable properties normally use `Option<T>`, which cannot distinguish an omitted property from an explicit `null`. `true` gives every such property a three-state `Presence<T>` instead. Request and response bodies share one type graph, so this applies to responses too. The default is `false`.

```toml
preserve-optional-nullable = true
```

### `nullability-overrides`

Properties to treat as nullable where the document does not say so, keyed by their pointer, with each value pinning the property's current declared type: `"boolean"`, `"object"`, `"array"`, `"number"`, `"string"` or `"integer"`. An override implies three-state presence for that property.

```toml
[nullability-overrides]
"/components/schemas/Patch/properties/nickname" = "string"
```

Generation fails if the pointer no longer exists, its declared type or shape changes, or the document makes it nullable itself; the entry must then be reviewed and removed or updated.

## Operations

### `media-types`

Which media type is primary at a content position that declares several, keyed by the pointer the `multi-media-type` diagnostic prints.

```toml
[media-types]
"/paths/~1dokumente/post/requestBody/content" = "multipart/form-data"
```

At a request body every declared media type keeps its own client method; the pick keeps the operation's bare name and is the one media type the generated server accepts. At a response it picks the single type the client decodes, and the request carries it as an `Accept` header. Without an entry, JSON comes first. An entry the document cannot honour is an error.

### `pagination`

Which operations paginate, and how. Declared, never detected: 62 of the 78 corpus documents paginate and no two agree on how to say so. Each entry is keyed by the operation's generated method name.

```toml
[pagination.list_pets]
cursor-param = "cursor"     # the query parameter, by its wire name
next-cursor = "next"        # where the next cursor is in the success response
items = "items"             # where the page's items are; their element type is the stream's
```

`next-cursor` and `items` are dotted member paths such as `meta.next`. Every name is checked against the document before anything is generated. The operation then gains a `stream()` beside its `send()`, and the generated crate depends on `futures-core` and `futures-util` only because you asked for one.

### `body-limit`

How many bytes of a request body a generated server reads into memory: a ceiling, so a server does not become a denial-of-service target because a description said `type: string`. The default is `2097152` (2 MiB).

```toml
body-limit = 8388608
```

## Extensions

Both are off by default, and a configuration that asks for neither generates no attribute, no impl and no dependency for them.

### `json-schema`

`schemars::JsonSchema` (schemars 1.x) implementations, **written from the wire contract rather than derived**, so a schema names each member by its wire name and describes unions the way the wire tags them, whichever serde strategy is configured.

```toml
[json-schema]
types = { only = ["read::UserPartnerApi"] }   # or "all", the default
feature = "schemars"                          # optional: `#[cfg(feature = "schemars")]`
```

### `extra-derives`

Derives progeny does not know, as paths, on the types each rule selects, optionally behind a feature of the consuming crate.

```toml
[[extra-derives]]
derives = ["::fixtures::Fixture"]
types = { only = ["Pet"] }
feature = "fixtures"
serde-attributes = "ignored"   # required: "ignored", or "read"
nested = true                  # the default; false derives on the selected types alone
```

`serde-attributes` has no default, because progeny cannot tell whether a derive reads `#[serde]` attributes. `"read"` moves exactly the types the rule lands on to the serde derive, where the attributes are the wire contract. Derives progeny emits itself — the typed set, serde's, and `JsonSchema` — are refused here.

### Selecting types

`types` in both tables is `"all"` or `{ only = [...] }`. A selector is a component name or pointer, optionally qualified: `Pet` is every form of the type, `read::Pet` what `types::read::Pet` names, and `strict::Pet` what `types::Pet` names. Every generated type the selected ones hold gets the same treatment, because the outer impl does not compile without them; an `extra-derives` rule with `nested = false` stops at the selected types. A selector that names nothing, or a form the type does not have, is an error.

## Diagnostics

### `deny`

Which [diagnostics]({{< relref "../diagnostics.md" >}}) fail the build, by action, by breakage class, or both, with reviewed findings let through by `[[deny.accept]]`.

| Key | Meaning |
| --- | --- |
| `actions` | Refuse any diagnostic taking one of these actions: `"repair"`, `"degrade"`, `"warn"`. |
| `classes` | Refuse any diagnostic of one of these [breakage classes]({{< relref "../diagnostics.md#breakage-classes" >}}). |
| `accept` | Reviewed findings: each a `class`, an `action`, the exact pointer `at` (`""` is the document root), and an optional `reason`. |

```toml
[deny]
actions = ["degrade"]

[[deny.accept]]
class = "unsupported-construct"
action = "degrade"
at = "/components/schemas/Thing/properties/p0"
reason = "the vendor's `not` is advisory"
```
