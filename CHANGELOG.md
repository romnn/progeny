# Changelog

## Unreleased

- **Breaking: a binary server response streams.** Its arm carries an `axum::body::Body` instead
  of a buffered `Vec<u8>` or `Bytes`; bytes already in memory convert with `.into()`.
- **Breaking: a server response's typed header group is boxed, like its body.** Answer with
  `headers: Box::new(…Headers { .. })` or `.into()`; many typed headers no longer trip
  `clippy::large_enum_variant`.
- **A server response enum builds its error reply from a status.** It implements
  `server::ErrorResponse<B>` where the operation's `default` and error arms share one body type.
- **Operations a query in their path key tells apart, such as `/responses?beta=true` beside
  `/responses`, are served as their own operations.** `Route::from_matched_with_query` resolves
  them, and keys the server cannot tell apart stay client-only with an `unregistrable-route`
  diagnostic.
- **`map = "hash-map"` compiles under lenient decoding.**
- **Generated crates build without warnings on Rust 1.88 and newer, in every feature
  selection.** Manifests declare `serde`, `serde_json`, `indoc` and `thiserror` only where the
  code names them, and the `dead_code`, `type_complexity` and paginated-client `nonminimal_bool`
  warnings are gone.
- **Multipart requests send what their encoding declares.** A union of strings is a text part, a
  string whose encoding says JSON is JSON, an explicit null in a JSON part is sent, and a
  multipart schema that a JSON body also references keeps its upload fields.
- **More unions get a typed shape instead of arbitrary JSON:** a numeric enum split across
  branches, a string enum beside open or restricted strings, a scalar beside tagged objects, a
  union of required-only branches, and a branch that flattens its own union.
- **A tuple is generated only at an arity Rust supports.** A longer one stays arbitrary JSON with
  a diagnostic.
- **A response type that a request also uses keeps capturing undeclared members.**
- **A struct field on a cycle that an enum already boxes is no longer boxed again.**
- **A status range declared twice with different casing gets one client match arm.**
- **Operation types no longer collide with the client and server support re-exports.**

## v0.0.5

- **Breaking: server operations take a typed request context and answer with declared response
  headers.** `Api` gains `type Context: FromRequestParts<Self>`, extracted before any decoding and
  passed to every operation and to `on_rejection`, which now returns a full `Response`. Each
  response variant carries its typed headers, with `raw` for the rest.
- **The client reads declared response headers.** An arm with headers yields `{ body, headers }`;
  strict decoding fails with `Error::HeaderDecode`, lenient decoding reports a degradation.
- **A `default` response refuses a status that another arm claims.** The server answers `500`.

## v0.0.4

- **Breaking: generated crates are edition 2024 with `rust-version = "1.88"`.**
- **Breaking: exact number literals need the `exact-numbers` feature.** The library no longer
  turns on `serde_json/arbitrary_precision` in a dependent's build; without the feature, a number
  the model holds only rounded is reported as `inexact-number`. The CLI enables it.
- **Breaking: every open enum's fallback variant is `Unknown`.** A declared `unknown` value
  becomes `Unknown_`.
- **Size integers by their format.** `formats.integers = "by-format"`.
- **Leave the document's prose out of doc comments.** `docs = "none"`.
- **Accept reviewed findings by location.** `deny.accept` lets a deny policy pass findings at named
  locations, and generation fails once an accepted entry no longer occurs.
- **Generate the types a JSON Schema bundle's roots reach.** `progeny::generate_schemas`.
- **An inline schema is named by its title instead of its position.**
- **Many more unions get a typed shape:** serde's externally tagged enums, self-naming unions
  whose branches omit or alias their tag, members declared beside a union, open string enums, a
  discriminator base, `allOf`/`oneOf` inheritance families, and every discriminator mapping value.
- **Recursive and cyclic schemas generate finite types, and every walk terminates.**
- **The generated server reads requests as the document declares them:** object parameters
  member by member, cookies from every `Cookie` header, `allowReserved` values intact, every
  content type a wildcard body admits, and multipart parts that arrive twice or with duplicate
  headers refused.
- **The generated server answers a JSON `null` as JSON, and a ranged response variant carries the
  status it is sent with.**
- **The client sends each operation to the server its path item or operation declares, and a
  parameter declared with JSON content as that JSON text.**
- **Time values from the `time` crate are held in a shipped RFC 3339 wrapper.**
  `formats.date-time = "time"`.
- **A number no `i64` or `f64` holds keeps its digits through the buffered decoder.**

## v0.0.3

- **Breaking: the client takes each operation's inputs as a params struct the caller builds.**
- **The client decodes responses leniently into read forms and reports what it tolerated.**
  `decoding = "strict"` refuses drift instead.
- **The client sends credentials:** API keys in the query, a header or a cookie, and bearer, basic
  and OAuth2 tokens. A client holds none when no operation requires a scheme.
- **The client bounds the response bodies it reads and keeps them for the caller.**
- **Breaking: a body that breaks off is `Error::BodyRead`, not `Error::Decode`.** It carries the
  status and the transport error.
- **No client error contains the requested URL,** so an API key in its query cannot leak.
- **Generate `schemars::JsonSchema` impls and extra derives for selected types.** `json-schema` and
  `extra-derives`.
- **Optional nullable properties can keep absent, `null` and a value apart.**
  `preserve-optional-nullable` and `nullability-overrides`.
- **Generated crates have an `operations` reflection module.** `operations::Route` maps a matched
  route back to its operation.
- **Breaking: a member's site in a degradation report is a `support::MemberSite`,** whose
  `member()` returns the wire name directly.
- **Releases publish the `progeny` and `cargo-progeny` binaries and Homebrew casks.**

## v0.0.2

- **Generate a Rust crate from an OpenAPI 3.0 or 3.1 description:** shared types, a `reqwest`
  client and an `axum` server, as one crate or a workspace of types, client and server crates.
- **Paginate the operations a configuration declares.** `pagination`, keyed by operation id.
- **Every place the generated code cannot honor the description is a diagnostic,** and a `deny`
  policy fails generation on the classes or actions it names.
- **Hand-written serde implementations by default, to cut compile time.**
  `serde-impl = "derive-always"` goes back to serde's derive.
