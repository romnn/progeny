---
title: Generated code
weight: 3
---

# Generated code

What progeny emits, how it is laid out, and the rule that decides what happens when a description says something the generated types cannot express exactly.

## Soundness: degrade outward, never inward

Every construct real documents use is classified into one closed set of shapes. What is *not* in that set has no general fallback: it becomes `serde_json::Value` with a diagnostic, which is the designed behaviour rather than a gap. Two rules keep that honest:

- **Degrade outward, never inward.** A keyword progeny does not turn into a type — `not`, `if`/`then`, `contains` — makes the generated type accept a *superset* of what the document allows. Accepting a payload the vendor would reject is a tolerance decision; rejecting one they would accept would be a bug in the generated client.
- **Nothing degrades quietly.** Every place that gives up says so with a [diagnostic]({{< relref "diagnostics.md" >}}).

The same rule runs through the rest of the generated surface. Every generated enum is open, so a value the description does not list lands in the type's `Unknown` arm with its bytes intact rather than failing the payload. An optional property that is also nullable collapses onto one `Option` by default, and the collapse is reported as `presence-collapse`; `preserve-optional-nullable` keeps all three states instead.

## Packaging

`packaging` chooses the shape of the output:

| `packaging` | Output |
| --- | --- |
| `"crate"` (default) | one crate: a `Cargo.toml` and a `src` tree, with the client and the server behind the `client` and `server` features |
| `"module"` | one file, `progeny.rs`, to `include!` from a build script or to check in |
| `"workspace"` | three crates, `<name>-types`, `<name>-client` and `<name>-server` |

The workspace's types crate has no HTTP features, and the two edge crates depend on its exact generated version, so Cargo feature unification cannot pull client or server dependencies into a types-only consumer. Publish in dependency order: types first, then client and server.

The boundary trades time for memory. Across the eight-document quick tier, three sequential crates take 61% more wall time than one crate, while cutting the worst rustc peak by 44–63% on every non-trivial document. Choose the workspace when peak memory or a types-only dependency boundary matters; keep the default crate when one artifact and the shortest clean build matter more.

Every generated crate is edition 2024 and declares `rust-version = "1.88"`.

## The modules

| Module | Contents |
| --- | --- |
| `types` | one Rust type per schema, in its strict form: every required member present, enforced at compile time |
| `types::read` | the read forms responses decode into, under lenient decoding |
| `operations` | the description's operation set as data: `Operation`, `Route` and `Method` |
| `client` | a `Client` with one method per operation |
| `server` | an `Api` trait to implement, `router()`, and a rejection envelope |

A description with nothing to call generates no `client`, `server` or `operations`. `emit` turns either edge off.

### Serde strategy

Types get hand-written `Serialize` and `Deserialize` implementations by default, wherever they are eligible. This is a compile-time decision and not a behavioural one: the two strategies are asserted equivalent on the wire by a differential harness, and they agree on every payload in the corpus. On the type layer the hand-written path uses 65–67% less CPU and 47–55% less peak RSS than the derive.

The hand-written path for structs buffers members before assigning them, which needs a self-describing format such as JSON, YAML or TOML. For `bincode` or `postcard`, set `serde-impl = "derive-always"`.

### Reading responses

A vendor's description is a promise the vendor does not always keep, so the generated client reads responses **leniently** by default. A member that is absent, `null`, or not what the description declared becomes `None`; a list element that cannot be read is left out; an enum value or union payload the description does not list lands in the type's `Unknown` arm; a member the description never declared is reported, and kept in `extra` where the type has a read form of its own. Every deviation is recorded, keyed by its place in the *description*, and returned beside the value:

```rust
let page = client.list_users().send().await?;
if page.is_degraded() {
    tracing::warn!(drift = %page.degradations(), "the vendor's response drifted from its description");
}
for user in page.into_value() {
    let Some(id) = user.id else { continue };
    …
}
```

A type only a response yields is written in its read form. One a request sends as well keeps its strict form in `types` and gains a read twin in `types::read` only where the two shapes differ. A response whose root is not what the description declares is still an error: there is no value to hand back.

`decoding = "strict"` decodes responses into the strict types and fails a response on a deviation such as a missing required member or a value of the wrong type. String enums and unions stay open in this mode too, keeping a value the description does not list in their fallback variant, and undeclared members follow `unknown-fields`, which ignores them by default. It is a choice made when the crate is generated, not a switch on a client: strict leaves out `types::read`, so the response types change with it.

## The client

Each operation is a method taking a params struct, returning a request whose `send()` produces a typed response per declared status. A required input is a plain field and an optional one an `Option`, so a missing input is a compile error at the call site. A further request media type the operation declares gets its own method (`_json`, `_multipart`, …), and a header the description does not declare rides one request through `.header(...)`.

- **Credentials.** Every security scheme progeny can send becomes a setter on the client, named after the scheme. A credential is sent only on the requests whose operation requires the scheme.
- **Body limits.** `response_body_limit` bounds the body a client reads, on the client or on one request. A body past the limit is `Error::BodyTooLarge`.
- **Raw bodies.** `keep_raw_body()` keeps the bytes the server sent beside the decoded value.
- **Observers.** `observe` hands every degraded response, with its operation, status and report, to one place in the application. progeny itself logs nothing.
- **Pagination.** An operation declared under [`pagination`]({{< relref "reference/configuration.md#pagination" >}}) gains a `stream()` beside its `send()`.

The client has no callback or hook system: supply a preconfigured `reqwest::Client` for transport-wide authentication, retry, and tracing.

## The server

The server is an `Api` trait with one method per operation, and an `axum` router that dispatches to it. A handler is emitted only for routes a router will actually accept, which is asked of `matchit` rather than guessed from the template's shape; a route it refuses is reported as `unregistrable-route`. The request body a server reads is capped by `body-limit`, 2 MiB by default.

## Operations as data

The `operations` module is the description's operation set as Rust data, rendered from the same model as `client` and `server`, so the three cannot disagree:

```rust
use api::operations::{Method, Operation, Route};

for operation in Operation::ALL {
    println!("{} {}", operation.method().as_str(), operation.path());
}

assert_eq!(Route::ShowPetById.path(), "/pets/{petId}");
let route = Route::from_matched(Method::from_token("GET")?, "/pets/{petId}");
assert_eq!(route, Some(Route::ShowPetById));
```

`Operation::ALL` is exhaustive on purpose: a table keyed on it stops compiling when the description gains, loses, or renames an operation. `Route` names only the routes `server::router()` registers.

## Doc comments

The `title`, `summary` and `description` the document writes become doc comments, as the vendor wrote them. `docs = "none"` leaves the document's prose out; `#[deprecated]` and the doc comments progeny writes itself stay.
