//! The typed customization set.
//!
//! Everything a caller can say, as a closed set of typed values. There is no string-typed
//! passthrough field anywhere in it, which is what makes an unsupported customization a config
//! parse error rather than a latent codegen bug: the opaque-token injection path that let a
//! caller change a wire contract behind the generator's back does not exist here.
//!
//! [`ExtraDerives`] is the one place a caller names Rust it wrote, and it names *derive paths*,
//! validated as paths, never attributes. A derive adds an implementation and cannot change the
//! serde one: one that tried would collide with progeny's and fail to compile, which is loud. The
//! remaining way a derive can disagree with the wire, reading `#[serde(...)]` attributes a
//! hand-written type does not carry, is a ruling the caller has to state in
//! [`ExtraDerives::serde_attributes`] rather than one progeny assumes.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::diag::{Action, BreakageClass, Diagnostic, JsonPointer};

/// How progeny should generate, and what the caller refuses to accept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Which diagnostics the caller treats as build failures.
    #[serde(default)]
    pub deny: Deny,
    /// Which halves of the interface to emit.
    #[serde(default)]
    pub emit: Emit,
    /// Which crates the generated types use for the formats that have a choice.
    #[serde(default)]
    pub formats: Formats,
    /// Which map type generated code uses.
    #[serde(default)]
    pub map: MapKind,
    /// Derives to add to every generated type, where the type is eligible for them.
    #[serde(default)]
    pub derives: BTreeSet<Derive>,
    /// Derives to add to named types, where the caller asked for them by name.
    ///
    /// Unlike [`Config::derives`], an ineligible request here is an error rather than a note: the
    /// caller named a type and stated an intent that cannot be honoured.
    #[serde(default)]
    pub type_derives: BTreeMap<String, BTreeSet<Derive>>,
    /// What to do with members a payload has and the document did not declare.
    ///
    /// A rule for *strict* decoding: the request bodies a generated server reads, and the strict
    /// form of every type. The read forms a lenient client decodes into always keep undeclared
    /// members, whatever this says — see [`Decoding`].
    #[serde(default)]
    pub unknown_fields: UnknownFields,
    /// The same, for named types.
    #[serde(default)]
    pub type_unknown_fields: BTreeMap<String, UnknownFields>,
    /// Whether optional and nullable properties retain absent, null, and value as distinct states.
    ///
    /// Generated request and response bodies share one type graph, so this applies to every such
    /// property rather than splitting shared schemas by direction.
    #[serde(default)]
    pub preserve_optional_nullable: bool,
    /// Schema properties to treat as nullable, keyed by their JSON Pointer.
    ///
    /// Each value pins the property's declared type.
    /// Generation fails if the pointer disappears, its declaration changes shape, or it becomes
    /// nullable in the document, so a specification refresh cannot silently invalidate the
    /// override.
    ///
    /// An entry also preserves all three presence states for that property, without requiring
    /// [`Config::preserve_optional_nullable`].
    #[serde(default)]
    pub nullability_overrides: BTreeMap<String, SchemaType>,
    /// Names to use instead of the ones progeny derives, keyed by component name or by the
    /// schema's JSON Pointer.
    ///
    /// An explicit name is a declaration of identity: two shapes named here never merge with each
    /// other, even when they are structurally identical.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// Which media type wins at a content position that declares several, keyed by the
    /// position's JSON Pointer — the same pointer the `multi-media-type` diagnostic prints, e.g.
    /// `/paths/~1dokumente/post/requestBody/content`.
    ///
    /// The two halves of a position differ. At a **request body**, every declared media type is
    /// generated as its own client method; the entry picks the *primary* — the method that keeps
    /// the operation's bare name, and the only media type the generated server accepts — while
    /// the other declared types keep their suffixed siblings (`_json`, `_multipart`, …). At a
    /// **response**, the media type is the server's choice: the entry picks the single type the
    /// client decodes, and the request carries it as an `Accept` header so the server hears the
    /// preference; the types not decoded are reported. Without an entry, either position gets
    /// the fixed preference order (JSON first). Like [`Config::type_derives`], a request that
    /// cannot be honoured — a pointer where the document declares no content, or a media type
    /// the position does not declare — is an error rather than a note.
    #[serde(default)]
    pub media_types: BTreeMap<String, String>,
    /// Which `Deserialize`/`Serialize` implementation strategy to use.
    #[serde(default)]
    pub serde_impl: SerdeImpl,
    /// How a generated client decodes what it receives. See [`Decoding`].
    #[serde(default)]
    pub decoding: Decoding,
    /// Whether to emit a crate, a workspace, or a module tree.
    #[serde(default)]
    pub packaging: Packaging,
    /// What to call the emitted crate, when emitting one.
    #[serde(default)]
    pub package: Package,
    /// How much of a request body a generated server will read into memory.
    #[serde(default)]
    pub body_limit: BodyLimit,
    /// Which operations paginate, and how, keyed by operation id.
    ///
    /// Declared and never detected. 62 of the corpus's 78 documents paginate and **no two agree**
    /// on how to say so — the cursor parameter is called `offset` 541 times, `page` 319, `cursor`
    /// 213, `after` 198, and on through `page_token`, `page[cursor]` and `PageToken` — so detection
    /// would be a table of vendor spellings pretending to be a rule. That is what the predecessor
    /// did and what did not generalize. See [`Pagination`].
    #[serde(default)]
    pub pagination: BTreeMap<String, Pagination>,
    /// Derives from outside the typed [`Derive`] set, on the types each rule selects and on every
    /// generated type those reach. Empty by default. See [`ExtraDerives`].
    #[serde(default)]
    pub extra_derives: Vec<ExtraDerives>,
    /// `schemars::JsonSchema` implementations written from the wire contract. Off by default.
    /// See [`JsonSchemaImpls`].
    #[serde(default)]
    pub json_schema: Option<JsonSchemaImpls>,
}

/// Which generated types a rule applies to.
///
/// Whatever is selected, every generated type it reaches is included as well: its fields, list
/// items and map values, its union variants, the anonymous types written inline in it, and the
/// read forms a read form names. An implementation on the outer type does not compile without one
/// on each of those, so the closure is computed from the type graph rather than left to the
/// caller to list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeSelection {
    /// Every generated type.
    #[default]
    All,
    /// The types these selectors name, and everything they reach.
    ///
    /// A selector is a key under the grammar every type-keyed map uses — a `components.schemas`
    /// name such as `Pet`, or a JSON Pointer such as `/components/schemas/Pet` — optionally
    /// qualified by the module the wanted form lives in:
    ///
    /// * `Pet` selects every form of the type: its strict form and its read twin when it has
    ///   both, and otherwise the one type there is.
    /// * `read::Pet` selects what `types::read::Pet` names: the read twin, a type only responses
    ///   reach, or a shared type the `read` module re-exports. A type with no read form — one
    ///   only requests reach, or any type under [`Decoding::Strict`] — is an error.
    /// * `strict::Pet` selects what `types::Pet` names: the strict form, or a shared type. A type
    ///   only responses reach has none, and is an error.
    ///
    /// The part after the qualifier is the component name or pointer, as everywhere else in the
    /// configuration, not the Rust name. A selector that names nothing, and an empty list, are
    /// errors rather than rules that quietly apply to nothing.
    Only(Vec<String>),
}

/// Derives from outside progeny's typed [`Derive`] set, on the types a rule selects.
///
/// The open counterpart of [`Config::type_derives`], for derive macros progeny does not know:
/// a caller's own trait, a test-data generator, a schema crate. Each derive lands on every selected
/// type and, unless [`ExtraDerives::nested`] is off, every generated type those reach (see
/// [`TypeSelection`]), skipping type aliases, which cannot carry one. Types progeny does not generate — scalars, `String`, the standard containers,
/// `serde_json::Value`, the configured date, UUID and map crates, and the support types
/// `Presence` and `Upload` — get nothing, so the derived trait must already be implemented for
/// those a selected type holds, or the generated source does not compile.
///
/// A rule names no dependency: the crate the generated source is compiled in has to provide each
/// derive's crate. That is the consumer's own manifest under [`Packaging::Module`]; a generated
/// manifest declares only the gating [`ExtraDerives::feature`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ExtraDerives {
    /// The derive macros, as paths written inside `#[derive(...)]`: `::my_crate::Fixture`.
    ///
    /// Each must parse as a path without generic arguments. The derives progeny itself owns are
    /// refused here, because a second copy would conflict with the one progeny emits: the typed
    /// set belongs in [`Config::derives`] and [`Config::type_derives`], serde's two are progeny's
    /// serde strategy, and `schemars::JsonSchema` is [`Config::json_schema`], which knows the wire
    /// format where a derive would have to guess it from attributes.
    pub derives: Vec<String>,
    /// Which types get them, before the closure over everything those reach.
    #[serde(default)]
    pub types: TypeSelection,
    /// Whether the derives also land on every generated type the selected ones reach. On by
    /// default.
    ///
    /// Off, they land on the selected types alone; a selected alias still passes them to the type
    /// it names, since an alias cannot carry a derive itself. The derive then compiles only if
    /// every generated type a selected one holds already implements the trait — through another
    /// rule, or through progeny's own derives — which is the caller's to arrange.
    #[serde(default = "yes")]
    pub nested: bool,
    /// A cargo feature of the crate the generated source is compiled in, to put the derives
    /// behind: `#[cfg_attr(feature = "…", derive(…))]`. Ungated when absent.
    ///
    /// The gate is evaluated in the *consuming* crate, so a build that leaves the feature off
    /// compiles none of these impls. A generated manifest declares the feature; under
    /// [`Packaging::Module`] the including crate must declare it.
    #[serde(default)]
    pub feature: Option<String>,
    /// Whether these derives read `#[serde(...)]` attributes. Required, with no default.
    ///
    /// progeny cannot tell. A type on the hand-written serde path carries no serde attributes,
    /// so a derive that reads them sees Rust field names where the wire has its own, and no
    /// tagging where the wire has one: it would describe a different type than the one on the
    /// wire, which is the forbidden failure mode. The caller knows what their derive reads and
    /// says so here.
    pub serde_attributes: SerdeAttributes,
}

/// What a set of [`ExtraDerives`] makes of `#[serde(...)]` attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SerdeAttributes {
    /// The derives never look at them, so the serde strategy stays as configured.
    ///
    /// The caller vouches for this: progeny has no way to check it. A derive that does read them
    /// and is declared `ignored` describes Rust names on every hand-written type.
    Ignored,
    /// The derives read them, so every type the rule lands on takes the serde derive path, where
    /// the attributes are the wire contract.
    ///
    /// That costs compile time on exactly those types, and it is refused for the kinds no serde
    /// attribute can describe — an open string enum and a union whose variants carry their own
    /// tag — because there is no attribute set that would tell such a derive the truth.
    Read,
}

/// `schemars::JsonSchema` implementations for generated types.
///
/// Written by hand from the wire contract rather than derived, which is what makes them correct
/// on every type: the derive learns the wire format from `#[serde(...)]` attributes, and a type on
/// the hand-written serde path carries none. A schema here names each member by its wire name,
/// requires what the description requires, describes a union the way it is tagged on the wire, and
/// includes every open enum's fallback — so a value the type writes validates, and the schema says
/// as much about a payload as the type does.
///
/// The impls are schemars 1.x's, and the only crate they name besides `serde_json`: every format,
/// container and support type is described inline from the contract, so no schemars feature for
/// `chrono`, `uuid` or `indexmap` is needed. A schema's name is the type's path under `types` —
/// `Pet`, and `read.Pet` for the read form — so a strict type and its read twin never collide.
///
/// A read form is described as the well-formed payload: every member optional, undeclared members
/// allowed through `additionalProperties`, each member typed as the description declares it. The
/// lenient decoder accepts more than that — a member that is `null` or unreadable, a list element
/// it skips — and reports each one, and a validator built on the schema refuses those, which is
/// what a fixture or a seed payload wants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct JsonSchemaImpls {
    /// Which types get one, before the closure over everything those reach.
    #[serde(default)]
    pub types: TypeSelection,
    /// A cargo feature of the crate the generated source is compiled in, to put the impls behind:
    /// `#[cfg(feature = "…")]`. Ungated when absent.
    ///
    /// A generated manifest declares the feature with an optional `schemars` dependency behind
    /// it; under [`Packaging::Module`] the including crate declares both.
    #[serde(default)]
    pub feature: Option<String>,
}

/// How one operation paginates.
///
/// Every field is validated against the document when generating: a name that does not resolve is
/// a hard error rather than a generated method that cannot work. The caller stated an intent about
/// a specific operation, so a silent no-op would be the worst of the three options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Pagination {
    /// The query parameter carrying the cursor, by its **wire** name.
    ///
    /// The wire name rather than the Rust one, because it is the document that says `page[cursor]`
    /// and the configuration should quote the document rather than progeny's rendering of it.
    pub cursor_param: String,
    /// Where the next cursor is in the success response, as a dotted member path.
    ///
    /// `next_cursor`, or `meta.next` — the members are wire names, and each one is resolved
    /// against the type the response actually has.
    pub next_cursor: String,
    /// Where the page's items are, as a dotted member path. Its element type becomes the stream's.
    pub items: String,
}

/// How much of a request body a generated server will read into memory, in bytes.
///
/// A *ceiling*, and a decision rather than a default: a generated server must not become a
/// denial-of-service target because a description said `type: string`. `axum`'s `DefaultBodyLimit`
/// does not raise it — that layer inserts an extension only extractors calling `with_limited_body`
/// consult, and the generated support code reads the body with `to_bytes` and this number — so this
/// is the knob, and it exists because saying so in a comment was not enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct BodyLimit(pub usize);

impl Default for BodyLimit {
    fn default() -> Self {
        // Two mebibytes. Large enough for the request bodies this corpus describes, small enough
        // that an unauthenticated caller cannot make a server hold much.
        Self(2 * 1024 * 1024)
    }
}

/// Which halves of the interface to emit.
///
/// Separate features in the emitted crate, because compile time is a property of the product: a
/// server implementation must not pay for the client and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Emit {
    /// The shared type layer. Nothing else is useful without it.
    #[serde(default = "yes")]
    pub types: bool,
    /// The calling side.
    #[serde(default = "yes")]
    pub client: bool,
    /// The serving side.
    #[serde(default = "yes")]
    pub server: bool,
}

fn yes() -> bool {
    true
}

impl Default for Emit {
    fn default() -> Self {
        Self {
            types: true,
            client: true,
            server: true,
        }
    }
}

/// Which crates the generated types use for the formats that have a choice.
///
/// The defaults are the dependency-free ones. A generated crate should not acquire a dependency
/// the caller did not ask for — compile cost is a first-class output property, and a `String`
/// timestamp is a smaller surprise than an unrequested `chrono` in the dependency tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Formats {
    /// `date-time`, `date` and `time`.
    #[serde(default)]
    pub date_time: DateTimeCrate,
    /// `uuid`.
    #[serde(default)]
    pub uuid: UuidCrate,
    /// Raw binary request and response bodies.
    ///
    /// A base64 value inside JSON remains `String`: the wire carries text, and choosing a byte
    /// representation does not implicitly choose a codec.
    #[serde(default)]
    pub bytes: BytesRepr,
}

/// Which crate holds the date and time types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DateTimeCrate {
    /// No dependency: the RFC 3339 text, unparsed.
    #[default]
    String,
    /// `chrono::DateTime<chrono::Utc>`, `chrono::NaiveDate`, `chrono::NaiveTime`.
    Chrono,
    /// `time::OffsetDateTime`, `time::Date`, `time::Time`.
    Time,
    /// `jiff::Timestamp`, `jiff::civil::Date`, `jiff::civil::Time`.
    Jiff,
}

/// Which crate holds the UUID type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UuidCrate {
    /// No dependency: the text form, unparsed.
    #[default]
    String,
    /// `uuid::Uuid`.
    Uuid,
}

/// How byte payloads are held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BytesRepr {
    /// No dependency.
    #[default]
    Vec,
    /// `bytes::Bytes`.
    Bytes,
}

/// Which map type generated code uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MapKind {
    /// Deterministic iteration, no dependency. The default, because determinism is an output
    /// invariant everywhere else in progeny too.
    #[default]
    BTreeMap,
    /// Faster lookup, non-deterministic iteration.
    HashMap,
    /// Preserves the order members arrived in.
    IndexMap,
}

/// A derive progeny will put on a generated type.
///
/// Closed, and every member is **attribute-blind**: none of them reads `#[serde(...)]`. That is
/// load-bearing rather than incidental. When the hand-written serde path is selected the type
/// carries no serde attributes at all, so a derive that read them — `schemars::JsonSchema` honours
/// `rename`, `skip_serializing_if` and the tagging attributes — would emit a description of a
/// different type than the one on the wire. That is the forbidden failure mode, so the set is
/// closed against it by construction.
///
/// A derive outside the set goes through [`ExtraDerives`], which makes the caller rule on exactly
/// that question, and a JSON Schema through [`Config::json_schema`], which answers it from the
/// wire contract instead of from attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Derive {
    /// `Clone`.
    Clone,
    /// `Debug`.
    Debug,
    /// `PartialEq`.
    PartialEq,
    /// `Eq`. Ineligible for any type holding a floating-point number.
    Eq,
    /// `Hash`. Ineligible for any type holding a floating-point number.
    Hash,
    /// `PartialOrd`.
    PartialOrd,
    /// `Ord`. Ineligible for any type holding a floating-point number.
    Ord,
    /// `Default`. Ineligible for an enum, which has no default variant to pick.
    Default,
    /// `Copy`. Ineligible for anything holding a `String`, a `Vec` or a map.
    Copy,
}

impl Derive {
    /// The name the derive is written with.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Clone => "Clone",
            Self::Debug => "Debug",
            Self::PartialEq => "PartialEq",
            Self::Eq => "Eq",
            Self::Hash => "Hash",
            Self::PartialOrd => "PartialOrd",
            Self::Ord => "Ord",
            Self::Default => "Default",
            Self::Copy => "Copy",
        }
    }
}

/// What to do with members a payload has and the document did not declare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnknownFields {
    /// Accept and discard them. What a client wants: a vendor adding a field must not break it.
    #[default]
    Ignore,
    /// Refuse the payload. What a server often wants.
    Deny,
    /// Keep them in a map on the type.
    Capture,
}

/// A JSON Schema type pinned by a nullability override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaType {
    /// `boolean`.
    Boolean,
    /// `object`.
    Object,
    /// `array`.
    Array,
    /// `number`.
    Number,
    /// `string`.
    String,
    /// `integer`.
    Integer,
}

impl SchemaType {
    /// The JSON Schema spelling written in a document.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Number => "number",
            Self::String => "string",
            Self::Integer => "integer",
        }
    }
}

/// Which `Deserialize`/`Serialize` implementation to emit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SerdeImpl {
    /// The derive, always.
    ///
    /// **The escape hatch, and it has to stay reachable**: the buffered path requires a
    /// self-describing format, and a caller may feed generated types to one that is not — `bincode`
    /// and `postcard` cannot tell this deserializer what a member is called. Set this and every type
    /// goes back to the derive. It is also the one-flag A/B the differential harness is built on.
    DeriveAlways,
    /// The hand-written implementation wherever the eligibility function allows it.
    ///
    /// The default, because the compile-time saving is the point of generating this code at all and
    /// a payoff behind a flag is not one. The focused type layer uses 65–67% less CPU and 47–55%
    /// less peak RSS than the derive; over the full generated types, client, and server surface the
    /// measured saving is 31–44% CPU and 22–37% peak RSS. Fieldless enums take a path that never
    /// buffers, so they keep working with any format; structs are the ones that need a
    /// self-describing one.
    #[default]
    HandWrittenWhereEligible,
}

/// How a generated client decodes a response body, and which types a response yields.
///
/// A description is a statement about a vendor's API, and the vendor's *responses* are where it
/// is wrong first: a member the description marks required arrives `null` for one record out of
/// ten thousand, an enum gains a value, a list holds one element of another shape. A closed
/// decode fails the whole response over that one record. A request, by contrast, is data the
/// caller builds, and a mistake there is answered `400` immediately, to input the caller
/// controls — so the two directions get different rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decoding {
    /// Decode as far as the payload allows, and report what did not match.
    ///
    /// The default. A response decodes into a *read form* of its type — every member optional,
    /// undeclared members kept — where a member that is absent, `null`, or unreadable becomes
    /// `None` and a list element that cannot be read is skipped. Every tolerated deviation is
    /// counted by its place in the description and returned beside the value, so a consumer can
    /// tell an incomplete list from a complete one and a vendor's drift shows up as a report
    /// rather than an outage. A type reached from both a request and a response gets a strict
    /// form for the request and a read twin in `types::read` for the response; the client's
    /// signatures say which is which, and the caller never picks.
    ///
    /// Only a body whose root is unusable — not JSON, or not the root shape the description
    /// declares — is still an error: leniency cannot conjure a value from nothing.
    #[default]
    Lenient,
    /// Decode exactly what the description declares, and fail the response on the first
    /// deviation.
    ///
    /// One type per schema, required members as plain fields in both directions, and no read
    /// forms: switching a generated crate between the two modes changes its response types, so
    /// this is a choice made when the crate is generated, not a switch on a client. The client's
    /// shape stays the same — a response still carries a degradation report and the client still
    /// takes an observer — but the report is always empty and the observer never called. The
    /// string enums and unions stay open in this mode too: a value the description does not
    /// list is kept in the fallback variant rather than refused, because keeping it loses
    /// nothing and writing it back is exact.
    Strict,
}

/// How to package the generated source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Packaging {
    /// A complete crate: a manifest and a `src` tree.
    #[default]
    Crate,
    /// A workspace containing `<name>-types`, `<name>-client`, and `<name>-server` crates.
    ///
    /// The types crate has no HTTP features, only the gates [`Config::json_schema`] and
    /// [`Config::extra_derives`] name; the edge crates pin its exact generated version and may be
    /// published after it.
    Workspace,
    /// One file to `include!` from a build script or to check in.
    ///
    /// The file is edition 2024 source, like the generated crates, so the crate that includes it
    /// has to be on edition 2024 and Rust 1.88 or later too.
    Module,
}

impl Package {
    /// Why this package cannot be rendered, when it cannot be.
    ///
    /// Rendering assumes both fields are well-formed: the name reaches `format_ident!`, which
    /// panics on anything that is not a legal identifier once `-` becomes `_`, and the version
    /// is written into a manifest cargo has to parse. Both are caller strings, so the check
    /// runs before any of that — a panic in generated-code assembly is never the right way to
    /// hear about a typo in a config file.
    pub(crate) fn validity(&self) -> Result<(), String> {
        let name_shape = !self.name.is_empty()
            && self
                .name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && self.name.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            });
        // The identifier check catches what the shape check cannot: a keyword. `match` is a
        // shape-legal crate name whose module spelling does not parse.
        let as_identifier = self.name.replace('-', "_");
        if !name_shape || syn::parse_str::<syn::Ident>(&as_identifier).is_err() {
            return Err(format!(
                "the configured package name `{}` is not a name cargo and rustc both accept; \
                 use ASCII letters, digits, `-` and `_`, starting with a letter, and avoid \
                 keywords",
                self.name
            ));
        }
        if !semver_shaped(&self.version) {
            return Err(format!(
                "the configured package version `{}` is not a semantic version cargo accepts; \
                 use `MAJOR.MINOR.PATCH` with optional `-prerelease` and `+build` parts",
                self.version
            ));
        }
        Ok(())
    }
}

/// Whether a version string has semver's shape: three numeric components, then optional
/// `-prerelease` and `+build` parts from semver's character set.
fn semver_shaped(version: &str) -> bool {
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    let (core, prerelease) = match rest.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (rest, None),
    };
    let numbers = core.split('.').collect::<Vec<_>>();
    if numbers.len() != 3
        || numbers
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    [prerelease, build].into_iter().flatten().all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
    })
}

/// What to call the emitted crate.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Package {
    /// The crate name.
    pub name: String,
    /// The crate version.
    pub version: String,
}

impl Default for Package {
    fn default() -> Self {
        Self {
            name: "api".to_owned(),
            version: "0.1.0".to_owned(),
        }
    }
}

/// A caller's strictness policy over diagnostics.
///
/// Strictness is caller policy rather than library judgement: [`crate::generate`] always returns
/// every diagnostic it produced, and the caller — a build script, a binary, CI — decides which
/// ones stop the build. A team that wants "no degradations in CI" says so here; a team
/// exploring a broken vendor spec says nothing and gets output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Deny {
    /// Refuse any diagnostic taking one of these actions.
    #[serde(default)]
    pub actions: BTreeSet<Action>,
    /// Refuse any diagnostic of one of these breakage classes.
    #[serde(default)]
    pub classes: BTreeSet<BreakageClass>,
    /// Reviewed findings the refusal lets through, written as `[[deny.accept]]`.
    ///
    /// This is how a vendored document with a few known degradations still fails the build on
    /// the next one.
    /// Each entry is matched against every occurrence before diagnostics are aggregated.
    /// An occurrence no entry names is therefore denied, when its class or action is, however
    /// many occurrences of its class the document has.
    #[serde(default)]
    pub accept: Vec<Accept>,
}

/// One finding the caller has reviewed: a breakage class and action at one document location.
///
/// The action is part of the finding, because one class can mean different things at one place:
/// a `$ref` that was repaired is not the same finding as the same `$ref` left unresolved.
/// The location is exact, so a new occurrence elsewhere, even inside the same schema, still
/// needs its own review.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Accept {
    /// The breakage class of the accepted finding.
    pub class: BreakageClass,
    /// What progeny did about it.
    pub action: Action,
    /// Where the finding is reported, as the JSON Pointer its diagnostic carries.
    ///
    /// `""` is the document root.
    #[serde(deserialize_with = "exact_pointer")]
    pub at: JsonPointer,
    /// Why the finding is acceptable, for the reader of the configuration.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Read an [`Accept::at`], refusing text that only resembles a JSON Pointer.
///
/// [`JsonPointer::parse`] is lenient about escapes, the way `$ref` reading needs: it reads `~2`
/// as a literal `~2`, which RFC 6901 spells `~02`.
/// Here the pointer is kept only when it renders back to exactly the text the caller wrote, so
/// every location has one spelling and a malformed one is a parse error.
fn exact_pointer<'de, D>(deserializer: D) -> Result<JsonPointer, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    JsonPointer::parse(&text)
        .filter(|pointer| pointer.to_string() == text)
        .ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{text}` is not a JSON Pointer: write \"\" for the document root, or start \
                 with `/` and escape `~` as `~0` and `/` as `~1`"
            ))
        })
}

/// How a configuration key names a thing in the document.
///
/// **One grammar for every keyed map**, parsed here and nowhere else. A key starting with `/` is a
/// JSON Pointer to where the thing is written — `/components/schemas/Pet`, `/paths/~1pets/get`.
/// Anything else is a name in the thing's own namespace: the `components.schemas` name for a type,
/// the generated method name for an operation. Before this type existed each keyed map grew its own
/// matcher, and `pagination` had drifted into a second dialect — the grammar was four
/// implementations claiming to be one convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Address<'a> {
    /// Where it is written: a JSON Pointer, recognized by its leading `/`.
    Pointer(&'a str),
    /// What it is called, in the namespace of the map the key appears in.
    Name(&'a str),
}

impl<'a> Address<'a> {
    pub(crate) fn parse(key: &'a str) -> Self {
        if key.starts_with('/') {
            Self::Pointer(key)
        } else {
            Self::Name(key)
        }
    }

    /// Whether this key names the thing called `name` written at `origin`.
    pub(crate) fn names(self, name: Option<&str>, origin: &str) -> bool {
        match self {
            Self::Pointer(pointer) => pointer == origin,
            Self::Name(named) => name == Some(named),
        }
    }
}

/// Which forms of a type a [`TypeSelection`] selector reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormFilter {
    /// Every form: the strict one and the read twin, or the one type there is.
    Every,
    /// What `types::read::Name` names.
    Read,
    /// What `types::Name` names.
    Strict,
}

/// One [`TypeSelection::Only`] selector, parsed: a form qualifier and an [`Address`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selector<'a> {
    pub(crate) form: FormFilter,
    pub(crate) address: Address<'a>,
}

impl<'a> Selector<'a> {
    pub(crate) fn parse(key: &'a str) -> Self {
        let (form, rest) = if let Some(rest) = key.strip_prefix("read::") {
            (FormFilter::Read, rest)
        } else if let Some(rest) = key.strip_prefix("strict::") {
            (FormFilter::Strict, rest)
        } else {
            (FormFilter::Every, key)
        };
        Self {
            form,
            address: Address::parse(rest),
        }
    }
}

/// The derives progeny emits itself, by the last segment of their path.
///
/// A second copy from an open rule would be a conflicting implementation, so each is refused with
/// the setting that owns it.
const OWNED_DERIVES: [(&str, &str); 12] = [
    ("Clone", "`derives` or `type-derives`"),
    ("Debug", "`derives` or `type-derives`"),
    ("PartialEq", "`derives` or `type-derives`"),
    ("Eq", "`derives` or `type-derives`"),
    ("Hash", "`derives` or `type-derives`"),
    ("PartialOrd", "`derives` or `type-derives`"),
    ("Ord", "`derives` or `type-derives`"),
    ("Default", "`derives` or `type-derives`"),
    ("Copy", "`derives` or `type-derives`"),
    ("Serialize", "`serde-impl`"),
    ("Deserialize", "`serde-impl`"),
    ("JsonSchema", "`json-schema`"),
];

/// The features a generated crate manifest declares itself, which a gate cannot reuse without
/// declaring the same key twice.
const GENERATED_FEATURES: [&str; 8] = [
    "default",
    "client",
    "server",
    "rustls",
    "native-tls",
    "http2",
    "charset",
    "system-proxy",
];

/// Whether a string is a feature name cargo accepts: ASCII letters, digits, `_`, `-`, `+` and
/// `.`, not starting with one of the last three.
fn feature_shaped(feature: &str) -> bool {
    feature
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric() || first == '_')
        && feature.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+' | '.')
        })
}

/// Why a derive path cannot be emitted, when it cannot be.
fn derive_path_validity(path: &str) -> Result<(), String> {
    let parsed = syn::parse_str::<syn::Path>(path).map_err(|_| {
        format!(
            "the configured extra derive `{path}` is not a Rust path; write it as it would \
             appear inside `#[derive(...)]`, such as `::my_crate::MyTrait`"
        )
    })?;
    if parsed
        .segments
        .iter()
        .any(|segment| !segment.arguments.is_none())
    {
        return Err(format!(
            "the configured extra derive `{path}` has generic arguments, which a derive path \
             cannot"
        ));
    }
    let last = parsed
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_default();
    if let Some((_, owner)) = OWNED_DERIVES.iter().find(|(name, _)| *name == last) {
        return Err(format!(
            "the configured extra derive `{path}` is one progeny emits itself; ask for it \
             through {owner}, or the two copies conflict"
        ));
    }
    Ok(())
}

impl Config {
    /// Why the open derive and JSON Schema settings cannot be rendered, when they cannot be.
    ///
    /// Checked before anything is generated, like [`Package::validity`]: each value ends up in
    /// emitted source or in a generated manifest, and a malformed one would surface as a parse
    /// error in a file the caller did not write.
    pub(crate) fn extension_validity(&self) -> Result<(), String> {
        let mut gates = Vec::new();
        for rule in &self.extra_derives {
            if rule.derives.is_empty() {
                return Err(
                    "an `extra-derives` rule lists no derives, so it would apply nothing"
                        .to_owned(),
                );
            }
            for path in &rule.derives {
                derive_path_validity(path)?;
            }
            gates.push(("extra-derives", rule.feature.as_deref(), &rule.types));
        }
        if let Some(schemas) = &self.json_schema {
            gates.push(("json-schema", schemas.feature.as_deref(), &schemas.types));
        }
        for (table, feature, types) in gates {
            if let TypeSelection::Only(selectors) = types
                && selectors.is_empty()
            {
                return Err(format!(
                    "the configuration's `{table}` selects an empty list of types, so it would \
                     apply to nothing; select `all` or name a type"
                ));
            }
            let Some(feature) = feature else {
                continue;
            };
            if !feature_shaped(feature) {
                return Err(format!(
                    "the configuration's `{table}` gates on `{feature}`, which is not a feature \
                     name cargo accepts; use ASCII letters, digits, `_`, `-`, `+` and `.`"
                ));
            }
            if self.packaging == Packaging::Crate && GENERATED_FEATURES.contains(&feature) {
                return Err(format!(
                    "the configuration's `{table}` gates on `{feature}`, which the generated \
                     manifest already declares for itself; pick a feature of its own"
                ));
            }
        }
        Ok(())
    }

    /// Whether this optional and nullable property keeps all three presence states.
    pub(crate) fn preserves_optional_nullable_at(&self, address: &JsonPointer) -> bool {
        self.preserve_optional_nullable
            || self
                .nullability_overrides
                .contains_key(&address.to_string())
    }

    /// Whichever of these derives the caller asked for by name, for a type named either by its
    /// component name or by its JSON Pointer.
    pub(crate) fn derives_for(&self, component: Option<&str>, address: &str) -> &BTreeSet<Derive> {
        Self::keyed(&self.type_derives, component, address).unwrap_or(&self.derives)
    }

    /// The unknown-field policy for one type, falling back to the crate-wide one.
    pub(crate) fn unknown_fields_for(
        &self,
        component: Option<&str>,
        address: &str,
    ) -> UnknownFields {
        Self::keyed(&self.type_unknown_fields, component, address)
            .copied()
            .unwrap_or(self.unknown_fields)
    }

    /// The name the caller asked for, if any.
    pub(crate) fn name_for(&self, component: Option<&str>, address: &str) -> Option<&String> {
        Self::keyed(&self.names, component, address)
    }

    /// Look a type up by component name first, then by address.
    ///
    /// The [`Address`] grammar, with the name spelling given priority when both keys are present:
    /// both are accepted because both are how a caller thinks about a type — `Pet` for a component,
    /// and the pointer for a shape the document never named. [`Address::names`] is the one
    /// matcher, for the application path here exactly as for the validation path in
    /// `unmatched_keys`: a second implementation of the grammar once let the validator accept a
    /// key the lookups then silently ignored, which is the defect this module exists to prevent.
    /// The scan is over a caller-written table of at most a few dozen entries.
    fn keyed<'a, T>(
        table: &'a BTreeMap<String, T>,
        component: Option<&str>,
        address: &str,
    ) -> Option<&'a T> {
        let matched = |wanted: fn(Address<'_>) -> bool| {
            table.iter().find_map(|(key, entry)| {
                let parsed = Address::parse(key);
                (wanted(parsed) && parsed.names(component, address)).then_some(entry)
            })
        };
        matched(|parsed| matches!(parsed, Address::Name(_)))
            .or_else(|| matched(|parsed| matches!(parsed, Address::Pointer(_))))
    }

    /// Every key in the type-keyed maps that names nothing in `named`, with the map it sits in.
    ///
    /// A key that matches no shape is a typo, and honoring the rest of the configuration around it
    /// would be a silent no-op — the caller asked for a rename or a policy and got nothing, with
    /// nothing saying so. That is the exact defect class this module's charter exists to prevent,
    /// and `pagination` already refuses its unmatched keys; this brings the type maps to the same
    /// standard.
    pub(crate) fn unmatched_keys<'a>(
        &'a self,
        named: impl Fn(Address<'_>) -> bool,
    ) -> Vec<(&'a str, &'static str)> {
        let tables: [(&dyn Fn() -> Vec<&'a str>, &'static str); 3] = [
            (&|| self.names.keys().map(String::as_str).collect(), "names"),
            (
                &|| self.type_derives.keys().map(String::as_str).collect(),
                "type-derives",
            ),
            (
                &|| {
                    self.type_unknown_fields
                        .keys()
                        .map(String::as_str)
                        .collect()
                },
                "type-unknown-fields",
            ),
        ];
        let mut unmatched = Vec::new();
        for (keys, table) in tables {
            for key in keys() {
                if !named(Address::parse(key)) {
                    unmatched.push((key, table));
                }
            }
        }
        unmatched
    }

    /// The diagnostics this configuration refuses to accept.
    ///
    /// Pass the diagnostics generated under this same configuration.
    /// Whether a diagnostic is accepted is settled during generation, by
    /// [`Deny::accept`], so diagnostics generated under another configuration carry that
    /// configuration's acceptances.
    pub fn denied<'a>(
        &'a self,
        diagnostics: &'a [Diagnostic],
    ) -> impl Iterator<Item = &'a Diagnostic> {
        diagnostics.iter().filter(|diagnostic| {
            !diagnostic.accepted()
                && (self.deny.actions.contains(&diagnostic.action())
                    || self.deny.classes.contains(&diagnostic.class()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::diag::{Action, BreakageClass, Diagnostic, JsonPointer};
    use color_eyre::eyre::{self, OptionExt as _};

    fn diagnostic(class: BreakageClass, action: Action) -> Diagnostic {
        Diagnostic::new(class, action, JsonPointer::root(), "detail")
    }

    #[test_util::test]
    fn the_default_configuration_denies_nothing() {
        let config = Config::default();
        let diagnostics = [
            diagnostic(BreakageClass::WildUnion, Action::Degrade),
            diagnostic(BreakageClass::InvalidExample, Action::Warn),
        ];
        assert_eq!(config.denied(&diagnostics).count(), 0);
    }

    #[test_util::test]
    fn a_policy_is_read_from_the_names_diagnostics_render_with() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            [deny]
            actions = ["degrade"]
            classes = ["dangling-ref"]
        "#})?;
        let diagnostics = [
            diagnostic(BreakageClass::WildUnion, Action::Degrade),
            diagnostic(BreakageClass::DanglingRef, Action::Repair),
            diagnostic(BreakageClass::InvalidExample, Action::Warn),
        ];
        let denied: Vec<_> = config.denied(&diagnostics).collect();
        assert_eq!(denied.len(), 2);
        assert_eq!(denied[0].class(), BreakageClass::WildUnion);
        assert_eq!(denied[1].class(), BreakageClass::DanglingRef);
    }

    #[test_util::test]
    fn an_unsupported_knob_is_a_config_error_not_a_silent_no_op() {
        let error = toml::from_str::<Config>("attribute = \"#[serde(skip)]\"")
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test_util::test]
    fn an_unknown_diagnostic_name_is_a_config_error() {
        assert!(
            toml::from_str::<Config>(indoc::indoc! {r#"
                [deny]
                actions = ["reject"]"#})
            .is_err()
        );
        assert!(
            toml::from_str::<Config>(indoc::indoc! {r#"
                [deny]
                classes = ["whatever"]"#})
            .is_err()
        );
    }

    /// An accepted finding is read with its exact pointer, including the document root and
    /// escaped tokens, and its reason is optional.
    #[test_util::test]
    fn accepted_findings_are_read_with_exact_pointers() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            [deny]
            classes = ["wild-union"]

            [[deny.accept]]
            class = "wild-union"
            action = "degrade"
            at = "/paths/~1pets~1{id}/get"
            reason = "reviewed"

            [[deny.accept]]
            class = "missing-final-line-break"
            action = "repair"
            at = ""
        "#})?;
        let [pets, root] = config.deny.accept.as_slice() else {
            eyre::bail!("expected two entries, got {:?}", config.deny.accept);
        };
        assert_eq!(
            pets.at,
            JsonPointer::root()
                .child("paths")
                .child("/pets/{id}")
                .child("get")
        );
        assert_eq!(pets.reason.as_deref(), Some("reviewed"));
        assert!(root.at.is_root());
        assert_eq!(root.reason, None);
    }

    /// Text that is not exactly a JSON Pointer, or an entry without its action, is refused when
    /// the configuration is read rather than left to match nothing.
    #[test_util::test]
    fn a_malformed_accepted_finding_is_a_config_error() {
        for at in [
            "components/schemas/Pet",
            "/components/schemas/a~2b",
            "/trailing~",
        ] {
            let error = toml::from_str::<Config>(&indoc::formatdoc! {r#"
                [[deny.accept]]
                class = "wild-union"
                action = "degrade"
                at = "{at}"
            "#})
            .err()
            .ok_or_eyre("a malformed pointer should be refused")?;
            assert!(
                error.to_string().contains("not a JSON Pointer"),
                "{at}: {error}"
            );
        }
        assert!(
            toml::from_str::<Config>(indoc::indoc! {r#"
                [[deny.accept]]
                class = "wild-union"
                at = "/components/schemas/Pet"
            "#})
            .is_err()
        );
    }

    #[test_util::test]
    fn presence_and_nullability_override_configuration_is_typed() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            preserve-optional-nullable = true

            [nullability-overrides]
            "/components/schemas/Patch/properties/nickname" = "string"
        "#})?;
        assert!(config.preserve_optional_nullable);
        assert_eq!(
            config
                .nullability_overrides
                .get("/components/schemas/Patch/properties/nickname"),
            Some(&super::SchemaType::String)
        );
    }

    #[test_util::test]
    fn the_open_derive_and_json_schema_settings_are_read_from_toml() {
        let config: Config = toml::from_str(indoc::indoc! {r#"
            [json-schema]
            types = { only = ["read::UserPartnerApi"] }
            feature = "schemars"

            [[extra-derives]]
            derives = ["::fixtures::Fixture"]
            feature = "fixtures"
            serde-attributes = "ignored"
        "#})?;
        assert_eq!(
            config.json_schema,
            Some(super::JsonSchemaImpls {
                types: super::TypeSelection::Only(vec!["read::UserPartnerApi".to_owned()]),
                feature: Some("schemars".to_owned()),
            })
        );
        assert_eq!(
            config.extra_derives,
            [super::ExtraDerives {
                derives: vec!["::fixtures::Fixture".to_owned()],
                types: super::TypeSelection::All,
                nested: true,
                feature: Some("fixtures".to_owned()),
                serde_attributes: super::SerdeAttributes::Ignored,
            }]
        );
        config
            .extension_validity()
            .map_err(|reason| eyre::eyre!(reason))?;

        let flat: Config = toml::from_str(indoc::indoc! {r#"
            [[extra-derives]]
            derives = ["::fixtures::Fixture"]
            nested = false
            serde-attributes = "ignored"
        "#})?;
        assert!(
            flat.extra_derives.first().is_some_and(|rule| !rule.nested),
            "{flat:?}"
        );

        // The section alone is every type, ungated.
        let everything: Config = toml::from_str("[json-schema]")?;
        assert_eq!(
            everything.json_schema,
            Some(super::JsonSchemaImpls::default())
        );
        assert_eq!(Config::default().json_schema, None);
    }

    /// The ruling has no default: a derive progeny cannot inspect is never assumed to be blind.
    #[test_util::test]
    fn an_open_derive_must_say_whether_it_reads_serde_attributes() {
        let error = toml::from_str::<Config>(indoc::indoc! {r#"
            [[extra-derives]]
            derives = ["::fixtures::Fixture"]
        "#})
        .err()
        .ok_or_eyre("the test expects this operation to fail")?;
        assert!(error.to_string().contains("serde-attributes"), "{error}");
    }

    #[test_util::test]
    fn an_open_derive_or_gate_that_cannot_be_rendered_is_refused() {
        let rule = |path: &str, feature: Option<&str>| super::ExtraDerives {
            derives: vec![path.to_owned()],
            types: super::TypeSelection::All,
            nested: true,
            feature: feature.map(ToOwned::to_owned),
            serde_attributes: super::SerdeAttributes::Ignored,
        };
        let validity = |rule: super::ExtraDerives, packaging: super::Packaging| {
            Config {
                extra_derives: vec![rule],
                packaging,
                ..Config::default()
            }
            .extension_validity()
        };
        for (path, owner) in [
            ("Clone", "type-derives"),
            ("::core::hash::Hash", "type-derives"),
            ("serde::Serialize", "serde-impl"),
            ("::schemars::JsonSchema", "json-schema"),
        ] {
            let refused = validity(rule(path, None), super::Packaging::Module)
                .err()
                .ok_or_eyre(format!("`{path}` must be refused"))?;
            assert!(refused.contains(owner), "{refused}");
        }
        for path in ["not a path", "Fixture<T>", ""] {
            assert!(
                validity(rule(path, None), super::Packaging::Module).is_err(),
                "`{path}` must be refused"
            );
        }
        for feature in ["", "-fixtures", "has space", "quote\""] {
            assert!(
                validity(rule("Fixture", Some(feature)), super::Packaging::Module).is_err(),
                "`{feature}` must be refused"
            );
        }
        // A gate may not redeclare a feature the generated manifest has, and may name it when
        // the including crate owns the manifest.
        assert!(validity(rule("Fixture", Some("client")), super::Packaging::Crate).is_err());
        validity(rule("Fixture", Some("client")), super::Packaging::Module)
            .map_err(|reason| eyre::eyre!(reason))?;
        validity(
            rule("::a::b::Fixture", Some("test-data+1.x")),
            super::Packaging::Crate,
        )
        .map_err(|reason| eyre::eyre!(reason))?;

        let nothing = Config {
            json_schema: Some(super::JsonSchemaImpls {
                types: super::TypeSelection::Only(Vec::new()),
                feature: None,
            }),
            ..Config::default()
        };
        assert!(nothing.extension_validity().is_err());
        let no_derives = Config {
            extra_derives: vec![super::ExtraDerives {
                derives: Vec::new(),
                ..rule("Fixture", None)
            }],
            ..Config::default()
        };
        assert!(no_derives.extension_validity().is_err());
    }
}
