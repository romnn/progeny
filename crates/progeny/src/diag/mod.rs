//! Structured diagnostics: the record every deviation from the input document produces.
//!
//! Published API descriptions are broken in recurring, classifiable ways, so tolerance here
//! is a designed feature rather than accumulated patches: a closed taxonomy of actions, a
//! closed catalogue of breakage classes, and a record for every deviation. Silently wrong
//! output is the only forbidden failure mode — generating less, loudly, always beats
//! generating something plausible.

mod pointer;

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

pub use pointer::JsonPointer;

/// What progeny did about a deviation.
///
/// Rejection is deliberately absent: it is the `Err` channel of the generate entry point, so
/// a rejected document cannot carry a partial success and a degraded document cannot hide
/// behind a missing error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    /// A well-understood fix with confident semantics; output is faithful to evident intent.
    Repair,
    /// Represented less precisely than the document asked, but soundly.
    Degrade,
    /// Generation unaffected; the document has a smell worth surfacing.
    Warn,
}

impl Action {
    /// The stable lowercase name used by the JSON-lines rendering and by config deny-lists.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Repair => "repair",
            Self::Degrade => "degrade",
            Self::Warn => "warn",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// The way in which a document deviated from what progeny can represent exactly.
///
/// Closed, with **no catch-all variant**: a newly observed way for descriptions to be broken
/// has to become a variant here, which forces a decision about which action it gets, which
/// mechanism implements it, and which fixture pins it.
///
/// Some breakage classes the predecessor carried are absent because this architecture
/// dissolves rather than handles them — recursive inline enum expansion cannot occur in an
/// arena model with node identity, cookie parameters are simply a fourth parameter location,
/// and parameter optionality/ordering is dissolved by the builder-only interface. Those are
/// verified by fixtures, not by diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum BreakageClass {
    /// A member progeny interprets held a value of the wrong shape. The member is preserved
    /// verbatim among the node's uninterpreted members, and nothing downstream reads it.
    MalformedMember,
    /// A YAML document whose last line has no line break, ending inside a block scalar. The
    /// break is supplied, which is the only way to read the document at all, and clip chomping
    /// then keeps one newline the document did not write.
    MissingFinalLineBreak,
    /// A number the document states cannot be represented in JSON (YAML `.inf` / `.nan`).
    NonFiniteNumber,
    /// A `$schema` naming a dialect progeny does not implement. Carried losslessly and
    /// interpreted as 2020-12.
    UnsupportedDialect,
    /// `$dynamicRef` / `$dynamicAnchor`. Represented losslessly, resolved as though it were
    /// the plain `$ref` / `$anchor` form.
    DynamicScoping,
    /// A `$ref` that resolves to nothing, after the best-effort repairs.
    DanglingRef,
    /// A schema `type` value that is not one of the seven JSON Schema types.
    UnknownSchemaType,
    /// A child schema named by the discriminator mappings of more than one parent.
    MultiParentDiscriminator,
    /// A discriminator whose mappings are incomplete or name variants implicitly.
    DiscriminatorEdgeCase,
    /// A `discriminator` and its `mapping` on a schema that declares no `oneOf` or `anyOf`: the
    /// inheritance spelling of a union, read as the union its mapping names.
    InheritanceDiscriminator,
    /// A union whose "any combination may match" semantics has no faithful Rust type.
    WildUnion,
    /// An operation whose method name the document did not choose: it declared no `operationId`,
    /// or declared one that sanitizes to a name another operation already has.
    ///
    /// Aggregated, because the first case is a property of a document rather than of an operation —
    /// 1,052 of the corpus's 1,058 records are it, and a record each would be a snapshot nobody
    /// reads. A genuine collision names the two identifiers and so stays its own record, which
    /// falls out of aggregating on the sentence rather than on the class alone.
    CollidingOperationId,
    /// An operation declares a switching-protocols response, but the generated HTTP surfaces do
    /// not hand the upgraded connection to either side.
    ConnectionUpgrade,
    /// A (location, style, explode, shape) parameter combination OpenAPI leaves undefined.
    QuerySerializationStyle,
    /// An example payload that contradicts its own schema. Never gates generation.
    InvalidExample,
    /// A `default` value that does not typecheck against its own schema.
    InvalidDefault,
    /// A route the generated router could not register, one that collides with another, or a
    /// path template and its declared parameters that disagree in either direction.
    UnregistrableRoute,
    /// The draft-04 tuple form `items: [A, B]`, which 2020-12 spells `prefixItems`.
    LegacyTupleItems,
    /// The 3.0 boolean `exclusiveMinimum`/`exclusiveMaximum` form in a document that declares
    /// 3.1, where a boolean there is never valid.
    LegacyExclusiveBound,
    /// The 3.0 `format: byte`/`format: binary` spelling in a document that declares 3.1, where
    /// 2020-12 defines neither and keeps both facts on other members.
    LegacyStringFormat,
    /// The draft-03 `required: true` flag written on a property, which every later draft and both
    /// OpenAPI dialects spell as an array on the property's parent.
    LegacyRequiredFlag,
    /// A 3.0 union branch whose only content is `nullable: true`: the one spelling 3.0 has for the
    /// null arm of a union, read literally as a branch that constrains nothing at all.
    NullableUnionBranch,
    /// A position declaring several media types, of which progeny generates one.
    ///
    /// Not a defect in the document: declaring a body in two encodings is legal and sometimes
    /// useful. It is a `Degrade` because generating one faithful body beats generating several
    /// half-faithful ones, and the alternates are named so a caller who needs one knows progeny
    /// saw it and chose another.
    MultiMediaType,
    /// A schema construct progeny does not interpret — `not`, `if`/`then`/`else`,
    /// `dependentSchemas`, `unevaluated*`, non-uniform `patternProperties`, a mixed-type `enum`.
    /// Held losslessly and typed as `serde_json::Value`.
    UnsupportedConstruct,
    /// An `allOf` whose branches cannot be merged into one shape: irreconcilable `type`s, or one
    /// property given two incompatible schemas.
    IrreconcilableAllOf,
    /// A property that is both optional and nullable, collapsed onto one `Option`, so "absent"
    /// and "present and null" become the same value.
    PresenceCollapse,
    /// A `readOnly` or `writeOnly` property in a type that crosses the direction the marker
    /// excludes: the one generated type serves both directions, so the member travels where the
    /// document says it does not.
    AccessCollapse,
    /// A schema sent as a `multipart/form-data` body and also used at another wire position:
    /// one schema is one Rust type, so its binary members keep the JSON spelling `String`
    /// everywhere, and the multipart parts carry the conventional filename rather than a
    /// caller-chosen one.
    EncodingCollapse,
    /// Two shapes whose names sanitize to the same Rust identifier.
    CollidingTypeName,
    /// A derive the caller asked every type for, on a type that cannot have it.
    UnsatisfiableDerive,
}

impl BreakageClass {
    /// Every variant, for the sweeps that need the closed set as data — the catalogue iterates
    /// this to force one pinned fixture per class.
    ///
    /// Completeness is held by a test beside this type: the derived `Deserialize` names every
    /// variant when it rejects an unknown one, and the test compares that list — serde's own,
    /// which cannot fall behind the enum — against this one, so a variant added without
    /// extending this list fails the build's tests instead of silently narrowing every sweep.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by the catalogue and the completeness test, which are test-only"
        )
    )]
    pub(crate) const ALL: [Self; 30] = [
        Self::MalformedMember,
        Self::MissingFinalLineBreak,
        Self::NonFiniteNumber,
        Self::UnsupportedDialect,
        Self::DynamicScoping,
        Self::DanglingRef,
        Self::UnknownSchemaType,
        Self::MultiParentDiscriminator,
        Self::DiscriminatorEdgeCase,
        Self::InheritanceDiscriminator,
        Self::MultiMediaType,
        Self::WildUnion,
        Self::CollidingOperationId,
        Self::ConnectionUpgrade,
        Self::QuerySerializationStyle,
        Self::InvalidExample,
        Self::InvalidDefault,
        Self::UnregistrableRoute,
        Self::LegacyTupleItems,
        Self::LegacyExclusiveBound,
        Self::LegacyStringFormat,
        Self::LegacyRequiredFlag,
        Self::NullableUnionBranch,
        Self::UnsupportedConstruct,
        Self::IrreconcilableAllOf,
        Self::PresenceCollapse,
        Self::AccessCollapse,
        Self::EncodingCollapse,
        Self::CollidingTypeName,
        Self::UnsatisfiableDerive,
    ];

    /// The stable kebab-case name used by the JSON-lines rendering and by config deny-lists.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::MalformedMember => "malformed-member",
            Self::MissingFinalLineBreak => "missing-final-line-break",
            Self::NonFiniteNumber => "non-finite-number",
            Self::UnsupportedDialect => "unsupported-dialect",
            Self::DynamicScoping => "dynamic-scoping",
            Self::DanglingRef => "dangling-ref",
            Self::UnknownSchemaType => "unknown-schema-type",
            Self::MultiParentDiscriminator => "multi-parent-discriminator",
            Self::DiscriminatorEdgeCase => "discriminator-edge-case",
            Self::InheritanceDiscriminator => "inheritance-discriminator",
            Self::MultiMediaType => "multi-media-type",
            Self::WildUnion => "wild-union",
            Self::CollidingOperationId => "colliding-operation-id",
            Self::ConnectionUpgrade => "connection-upgrade",
            Self::QuerySerializationStyle => "query-serialization-style",
            Self::InvalidExample => "invalid-example",
            Self::InvalidDefault => "invalid-default",
            Self::UnregistrableRoute => "unregistrable-route",
            Self::LegacyTupleItems => "legacy-tuple-items",
            Self::LegacyExclusiveBound => "legacy-exclusive-bound",
            Self::LegacyStringFormat => "legacy-string-format",
            Self::LegacyRequiredFlag => "legacy-required-flag",
            Self::NullableUnionBranch => "nullable-union-branch",
            Self::UnsupportedConstruct => "unsupported-construct",
            Self::IrreconcilableAllOf => "irreconcilable-all-of",
            Self::PresenceCollapse => "presence-collapse",
            Self::AccessCollapse => "access-collapse",
            Self::EncodingCollapse => "encoding-collapse",
            Self::CollidingTypeName => "colliding-type-name",
            Self::UnsatisfiableDerive => "unsatisfiable-derive",
        }
    }

    /// Whether one record stands for one occurrence or for every occurrence in the document.
    ///
    /// Some classes fire at a scale that would make the output useless — 16,100
    /// optional-and-nullable collapses and 652 draft-04 tuple forms in this corpus alone — and a
    /// record per occurrence turns the per-spec snapshot into noise nobody reads, which defeats
    /// its purpose as a review gate. So a class says here whether it aggregates, and the choice
    /// is made where the action is: adding a variant forces both decisions at once.
    ///
    /// The rule of thumb behind the assignments: aggregate when a reader wants the *count* and a
    /// handful of examples ("this document writes draft-04 tuples, 651 times"); keep every
    /// occurrence when a reader has to act on each one individually (a skipped route, a renamed
    /// operation).
    #[must_use]
    pub fn aggregation(self) -> Aggregation {
        match self {
            // Scale classes: the count is the finding.
            Self::MalformedMember
            | Self::NonFiniteNumber
            | Self::UnsupportedDialect
            | Self::DynamicScoping
            | Self::DanglingRef
            | Self::UnknownSchemaType
            | Self::WildUnion
            | Self::QuerySerializationStyle
            | Self::InvalidExample
            | Self::InvalidDefault
            | Self::LegacyTupleItems
            | Self::LegacyExclusiveBound
            | Self::LegacyStringFormat
            | Self::LegacyRequiredFlag
            | Self::InheritanceDiscriminator
            | Self::MultiMediaType
            | Self::NullableUnionBranch
            | Self::UnsupportedConstruct
            | Self::IrreconcilableAllOf
            | Self::PresenceCollapse
            | Self::AccessCollapse
            | Self::CollidingTypeName
            | Self::CollidingOperationId
            // A scale class since the shape layer learned to read a discriminator a `oneOf`
            // inherits from the base its branches share. A family written that way is named twice
            // — once by the base, once by each `oneOf` over it — so every one of its variants has
            // two parents by construction, and `kundenangaben` alone produced 126 records, four of
            // them byte-identical because a merged key takes its address from its first part.
            | Self::MultiParentDiscriminator
            // Moved here at stage 7, when the router turned it into a scale class. A refusal folds
            // on an explicit key ("router-refusal"), so a document with a habit is one record —
            // `twilio-api-v2010` puts `.json` after a path variable in 99 operations, `anthropic`
            // writes `?beta=true` into 41 templates — however the refusal sentence is worded this
            // year. A genuine collision names what it collided with and so stays its own record:
            // no key, and the sentence differs per route. The same split `colliding-operation-id`
            // has.
            | Self::UnregistrableRoute
            | Self::UnsatisfiableDerive => Aggregation::PerDocument,
            // The first can occur at most once per document by construction; each of the rest names
            // a distinct set of document locations a reader has to look at, with no useful count to
            // report instead.
            Self::MissingFinalLineBreak
            | Self::DiscriminatorEdgeCase
            | Self::ConnectionUpgrade
            // Each occurrence names one position whose schemas a reader would split; the count
            // is never the finding, and the collapse cannot fire at its siblings' scale.
            | Self::EncodingCollapse => Aggregation::PerOccurrence,
        }
    }
}

/// Whether a diagnostic stands for one occurrence or for a class of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Aggregation {
    /// One record per occurrence.
    PerOccurrence,
    /// One record per document, carrying the occurrence count and the first few locations.
    PerDocument,
}

impl fmt::Display for BreakageClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// One deviation from the input document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Diagnostic {
    class: BreakageClass,
    action: Action,
    location: JsonPointer,
    detail: String,
    related: Vec<JsonPointer>,
    occurrences: NonZeroU32,
    /// What this diagnostic folds with, when that should not be the sentence.
    ///
    /// Aggregation identity defaults to `(class, detail)`: two records fold when they say the same
    /// thing. That default makes the *wording* load-bearing — improving a message re-keys the
    /// aggregation and rewrites snapshots, which is exactly what happened when the router-refusal
    /// sentence gained its `matchit` attribution and thirteen documents re-recorded. A site whose
    /// message may evolve independently of what the record *is* sets this to a stable key, so the
    /// invariant lives in a field the compiler carries rather than in a comment folding has to obey.
    /// Never serialized: identity is not output.
    fold: Option<String>,
}

/// How many locations an aggregated diagnostic names before it stops collecting them.
///
/// The count is the finding for an aggregated class; the locations are there to make it
/// investigable, and a few are enough for that.
const RELATED_CAP: usize = 5;

impl Diagnostic {
    /// Record a deviation at `location`.
    ///
    /// `detail` is a human sentence saying what was found and what was done about it; it ends
    /// up verbatim in build output and in the checked-in per-spec snapshots, so it must be
    /// deterministic — no addresses, no timings, no iteration-order-dependent ordering.
    #[must_use]
    pub fn new(
        class: BreakageClass,
        action: Action,
        location: JsonPointer,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            class,
            action,
            location,
            detail: detail.into(),
            related: Vec::new(),
            occurrences: NonZeroU32::MIN,
            fold: None,
        }
    }

    /// Fold on `key` instead of on the sentence.
    ///
    /// For sites whose wording may improve independently of what the record is about — see the
    /// `fold` field for the incident that motivated it.
    #[must_use]
    pub fn folded_as(mut self, key: impl Into<String>) -> Self {
        self.fold = Some(key.into());
        self
    }

    /// The aggregation identity: the explicit key when one was set, the sentence otherwise.
    fn fold_key(&self) -> &str {
        self.fold.as_deref().unwrap_or(&self.detail)
    }

    /// Attach the other document locations this diagnostic is about, such as the second of
    /// two colliding operations.
    ///
    /// Repeats are dropped, order kept. A pointer listed twice carries no information a reader can
    /// use and reliably reads as a bug — which is how the merged-key addressing defect was found in
    /// the first place: a shape that is the classification of several schemas can legitimately
    /// contribute a pointer another part of the same record already named.
    #[must_use]
    pub fn with_related(mut self, related: impl IntoIterator<Item = JsonPointer>) -> Self {
        for pointer in related {
            if pointer != self.location && !self.related.contains(&pointer) {
                self.related.push(pointer);
            }
        }
        self
    }

    /// Which way the document was broken.
    #[must_use]
    pub fn class(&self) -> BreakageClass {
        self.class
    }

    /// What progeny did about it.
    #[must_use]
    pub fn action(&self) -> Action {
        self.action
    }

    /// Where in the document it was found.
    #[must_use]
    pub fn location(&self) -> &JsonPointer {
        &self.location
    }

    /// The human sentence describing the finding and the response.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Other document locations this diagnostic is about.
    #[must_use]
    pub fn related(&self) -> &[JsonPointer] {
        &self.related
    }

    /// How many times this finding occurred.
    ///
    /// Always 1 for a per-occurrence class. For an aggregated one this is the whole document's
    /// count, [`Diagnostic::location`] is the first occurrence, and [`Diagnostic::related`] holds
    /// the next few.
    #[must_use]
    pub fn occurrences(&self) -> NonZeroU32 {
        self.occurrences
    }

    /// Fold another occurrence of the same finding into this record.
    fn absorb(&mut self, location: JsonPointer) {
        // Saturating rather than wrapping: a document with four billion occurrences of one
        // finding is already told truthfully enough by the cap.
        self.occurrences = self.occurrences.checked_add(1).unwrap_or(self.occurrences);
        if self.related.len() < RELATED_CAP {
            self.related.push(location);
        }
    }

    /// The one-line JSON rendering used for the checked-in per-spec snapshots.
    ///
    /// Keys are emitted in a fixed order so a snapshot diff reads as a behaviour change
    /// rather than as churn.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"class\":");
        push_json_string(&mut out, self.class.slug());
        out.push_str(",\"action\":");
        push_json_string(&mut out, self.action.slug());
        out.push_str(",\"location\":");
        push_json_string(&mut out, &self.location.to_string());
        out.push_str(",\"detail\":");
        push_json_string(&mut out, &self.detail);
        if self.occurrences > NonZeroU32::MIN {
            out.push_str(",\"occurrences\":");
            out.push_str(&self.occurrences.to_string());
        }
        if !self.related.is_empty() {
            out.push_str(",\"related\":[");
            for (index, pointer) in self.related.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, &pointer.to_string());
            }
            out.push(']');
        }
        out.push('}');
        out
    }
}

/// The human-readable rendering, as it appears in build output.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let location = if self.location.is_root() {
            "<document root>".to_owned()
        } else {
            self.location.to_string()
        };
        write!(
            f,
            "{}: {} at {location}: {}",
            self.action, self.class, self.detail
        )?;
        if self.occurrences > NonZeroU32::MIN {
            write!(f, " ({} occurrences)", self.occurrences)?;
        }
        // A handful of pointers into a deeply nested schema is longer than the sentence they
        // qualify, so build output gets the count and the JSON-lines rendering keeps the list for
        // tooling. Up to two are short enough to be worth naming — which covers the classes where
        // the other location *is* the finding, such as two colliding operations.
        match self.related.len() {
            0 => {}
            1 | 2 => {
                let related: Vec<String> = self.related.iter().map(ToString::to_string).collect();
                write!(f, " (see also {})", related.join(", "))?;
            }
            more => write!(f, " (and {more} more locations)")?,
        }
        Ok(())
    }
}

/// One lowercase hexadecimal digit. `nibble` is masked by the caller, so the fallback is
/// unreachable rather than a guess.
fn hex_digit(nibble: u32) -> char {
    char::from_digit(nibble, 16).unwrap_or('0')
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // JSON forbids unescaped control characters. Everything below `0x20` is two hex
            // digits, so the escape is built rather than formatted.
            c if c < '\u{20}' => {
                let code = u32::from(c);
                out.push_str("\\u00");
                out.push(hex_digit(code >> 4));
                out.push(hex_digit(code & 0xf));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Why a document is unusable.
///
/// Rejection is a last resort and it is total: there are no partial rejections, so this is
/// the only failure mode that produces no output at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RejectKind {
    /// The bytes are not a JSON or YAML document.
    Unparsable,
    /// A YAML mapping key is a sequence or a mapping, so it has no unambiguous member name.
    NonScalarKey,
    /// The document root is not an object.
    NotAnObject,
    /// No `openapi` version member.
    MissingVersion,
    /// An `openapi` version progeny does not implement.
    UnsupportedVersion,
    /// Neither `paths` nor `webhooks`, so the document describes no operations.
    NoOperations,
    /// The configuration asks, by name, for something this document cannot be given.
    UnsatisfiableConfig,
}

impl RejectKind {
    /// The stable kebab-case name of this rejection reason.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Unparsable => "unparsable",
            Self::NonScalarKey => "non-scalar-key",
            Self::NotAnObject => "not-an-object",
            Self::MissingVersion => "missing-version",
            Self::UnsupportedVersion => "unsupported-version",
            Self::NoOperations => "no-operations",
            Self::UnsatisfiableConfig => "unsatisfiable-config",
        }
    }
}

/// The error returned when a document is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {}{}", .kind.slug(), .detail, RejectLocation(.location.as_ref()))]
pub struct RejectError {
    kind: RejectKind,
    detail: String,
    location: Option<JsonPointer>,
}

impl RejectError {
    /// Reject the document, giving the reason and a human sentence.
    #[must_use]
    pub fn new(kind: RejectKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            location: None,
        }
    }

    /// Attach the document location responsible.
    #[must_use]
    pub fn at(mut self, location: JsonPointer) -> Self {
        self.location = Some(location);
        self
    }

    /// Why the document is unusable.
    #[must_use]
    pub fn kind(&self) -> RejectKind {
        self.kind
    }

    /// The human sentence describing the reason.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Where in the document the reason was found, when that is meaningful.
    #[must_use]
    pub fn location(&self) -> Option<&JsonPointer> {
        self.location.as_ref()
    }
}

struct RejectLocation<'a>(Option<&'a JsonPointer>);

impl fmt::Display for RejectLocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.0
            && !location.is_root()
        {
            write!(f, " (at {location})")?;
        }
        Ok(())
    }
}

/// The diagnostic sink threaded through the front end, together with the document location
/// currently being read.
///
/// Every degradation and repair site takes `&mut Ctx` and pushes its record, so producing the
/// less-precise value and producing its diagnostic are one operation rather than two that can
/// drift apart.
#[derive(Debug, Default)]
pub(crate) struct Ctx {
    path: JsonPointer,
    diagnostics: Vec<Diagnostic>,
    /// Where each aggregated finding already sits in `diagnostics`, so folding an occurrence into
    /// it is a lookup rather than a scan over everything reported so far.
    ///
    /// Keyed by the sentence as well as the class: two different findings of one class stay two
    /// records, which is what keeps "`description` should be a string" from being merged with
    /// "`url` should be a string" just because both are malformed members.
    aggregated: BTreeMap<(BreakageClass, Action, String), usize>,
}

impl Ctx {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The location of `token` within the node currently being read.
    pub(crate) fn child(&self, token: &str) -> JsonPointer {
        self.path.child(token)
    }

    /// The location of the node currently being read.
    pub(crate) fn here(&self) -> &JsonPointer {
        &self.path
    }

    /// Read a child node with the location extended by `token`.
    pub(crate) fn scoped<T>(&mut self, token: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(token);
        let out = f(self);
        self.path.pop();
        out
    }

    /// Record a deviation, folding it into an existing record when its class aggregates.
    ///
    /// Aggregation happens here rather than in a pass afterwards so that a caller cannot
    /// accidentally emit 16,100 lines by reporting at the wrong level: the class decides, and
    /// every report site goes through this one function.
    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
        if diagnostic.class.aggregation() == Aggregation::PerOccurrence {
            self.diagnostics.push(diagnostic);
            return;
        }
        // The action is part of the identity, not a rider: `Config::denied` filters on it, and
        // two sites folding on one key while disagreeing about the action would hand the deny
        // policy whichever action happened to be reported first.
        let key = (
            diagnostic.class,
            diagnostic.action,
            diagnostic.fold_key().to_owned(),
        );
        if let Some(&index) = self.aggregated.get(&key)
            && let Some(existing) = self.diagnostics.get_mut(index)
        {
            existing.absorb(diagnostic.location);
            return;
        }
        self.aggregated.insert(key, self.diagnostics.len());
        self.diagnostics.push(diagnostic);
    }

    /// Record that `key` held a value progeny could not interpret. The caller keeps the
    /// member verbatim among the node's uninterpreted members.
    pub(crate) fn malformed(&mut self, key: &str, expected: &str) {
        let location = self.child(key);
        self.report(Diagnostic::new(
            BreakageClass::MalformedMember,
            Action::Degrade,
            location,
            format!(
                "`{key}` should be {expected}; kept the member verbatim and ignored it for generation"
            ),
        ));
    }

    pub(crate) fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};
    use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};

    /// `ALL` really is all of them, by serde's own count.
    ///
    /// The derived `Deserialize` names every variant when it rejects an unknown one — a list
    /// that cannot fall behind the enum. Extracting it here, once, is what lets everything else
    /// iterate `ALL` instead of parsing an error message at each site.
    #[test_util::test]
    fn the_class_list_is_complete() {
        let error = serde_json::from_value::<BreakageClass>(serde_json::json!("no-such-class"))
            .err()
            .ok_or_eyre("a class that does not exist should not deserialize")?;
        let message = error.to_string();
        let listed = message
            .split_once("expected one of ")
            .map(|(_, rest)| rest)
            .ok_or_else(|| eyre::eyre!("the variant list moved: {message}"))?;
        let named: std::collections::BTreeSet<&str> = listed
            .split(", ")
            .filter_map(|name| name.trim().split('`').nth(1))
            .collect();
        let all: std::collections::BTreeSet<&str> = BreakageClass::ALL
            .iter()
            .map(|class| class.slug())
            .collect();
        assert_eq!(named, all);
        assert_eq!(BreakageClass::ALL.len(), named.len());
    }

    /// `slug()` and the derived serde names are one table, not two that happen to agree.
    ///
    /// The deny lists deserialize class names through serde while build output prints them
    /// through `slug()`; a variant whose kebab-casing is not what `slug()` spelled would print a
    /// name the configuration file then rejects as unknown.
    #[test_util::test]
    fn every_slug_is_the_serde_name() {
        for class in BreakageClass::ALL {
            let parsed: BreakageClass = serde_json::from_value(serde_json::json!(class.slug()))
                .wrap_err_with(|| format!("`{}` is not the serde spelling", class.slug()))?;
            assert_eq!(parsed, class);
        }
        for action in [Action::Repair, Action::Degrade, Action::Warn] {
            let parsed: Action = serde_json::from_value(serde_json::json!(action.slug()))
                .wrap_err_with(|| format!("`{}` is not the serde spelling", action.slug()))?;
            assert_eq!(parsed, action);
        }
    }

    #[test_util::test]
    fn json_line_has_a_fixed_key_order_and_escapes_detail() {
        let diagnostic = Diagnostic::new(
            BreakageClass::MalformedMember,
            Action::Degrade,
            JsonPointer::root().child("info").child("title"),
            indoc::indoc! {r#"
                found "x"
                kept it"#},
        );
        assert_eq!(
            diagnostic.to_json_line(),
            r#"{"class":"malformed-member","action":"degrade","location":"/info/title","detail":"found \"x\"\nkept it"}"#
        );
    }

    #[test_util::test]
    fn a_related_pointer_is_listed_once_and_never_repeats_the_location() {
        let at = |token: &str| JsonPointer::root().child("components").child(token);
        let diagnostic = Diagnostic::new(
            BreakageClass::MultiParentDiscriminator,
            Action::Warn,
            at("Variant"),
            "detail",
        )
        .with_related([at("First"), at("First"), at("Second"), at("Variant")]);
        assert_eq!(
            diagnostic.related(),
            [at("First"), at("Second")],
            "a repeated pointer carries nothing and reads as a bug"
        );
    }

    #[test_util::test]
    fn json_line_omits_related_when_empty_and_emits_it_when_present() {
        let base = Diagnostic::new(
            BreakageClass::CollidingOperationId,
            Action::Repair,
            JsonPointer::root(),
            "renamed",
        );
        assert!(!base.to_json_line().contains("related"));

        let with_related = base.with_related([JsonPointer::root().child("paths").child("/pets")]);
        assert!(
            with_related
                .to_json_line()
                .ends_with(r#","related":["/paths/~1pets"]}"#),
            "{}",
            with_related.to_json_line()
        );
    }

    #[test_util::test]
    fn human_rendering_names_the_document_root() {
        let diagnostic = Diagnostic::new(
            BreakageClass::UnsupportedDialect,
            Action::Warn,
            JsonPointer::root(),
            "declared an unknown dialect",
        );
        assert_eq!(
            diagnostic.to_string(),
            "warn: unsupported-dialect at <document root>: declared an unknown dialect"
        );
    }

    #[test_util::test]
    fn control_characters_are_escaped_as_json_requires() {
        let diagnostic = Diagnostic::new(
            BreakageClass::MalformedMember,
            Action::Degrade,
            JsonPointer::root(),
            "bell\u{7}",
        );
        let line = diagnostic.to_json_line();
        // The rendering has to survive a JSON parser, or the snapshots are not JSON lines.
        let parsed: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(parsed["detail"], serde_json::json!("bell\u{7}"));
        assert!(line.contains("\\u0007"), "{line}");
    }

    #[test_util::test]
    fn scoped_reads_restore_the_previous_location() {
        let mut ctx = Ctx::new();
        ctx.scoped("components", |ctx| {
            ctx.scoped("schemas", |ctx| ctx.malformed("Pet", "an object"));
            // Back at `/components` now, or the next sibling read would be reported inside the
            // node that was just left.
            ctx.malformed("schemas", "an object");
        });
        ctx.malformed("openapi", "a string");

        let locations: Vec<String> = ctx
            .into_diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.location().to_string())
            .collect();
        assert_eq!(
            locations,
            ["/components/schemas/Pet", "/components/schemas", "/openapi"]
        );
    }

    #[test_util::test]
    fn a_class_that_fires_at_scale_becomes_one_record_with_a_count() {
        let mut ctx = Ctx::new();
        assert_eq!(
            BreakageClass::PresenceCollapse.aggregation(),
            super::Aggregation::PerDocument
        );
        for index in 0..8 {
            let location = JsonPointer::root().child("p").child(index.to_string());
            ctx.report(Diagnostic::new(
                BreakageClass::PresenceCollapse,
                Action::Degrade,
                location,
                "collapsed",
            ));
        }
        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].occurrences().get(), 8);
        // The first occurrence is the location; the next few are related, and then it stops
        // collecting them.
        assert_eq!(diagnostics[0].location().to_string(), "/p/0");
        assert_eq!(diagnostics[0].related().len(), super::RELATED_CAP);
        assert!(
            diagnostics[0].to_json_line().contains(r#""occurrences":8"#),
            "{}",
            diagnostics[0].to_json_line()
        );
    }

    #[test_util::test]
    fn two_findings_of_one_aggregated_class_stay_two_records() {
        let mut ctx = Ctx::new();
        ctx.malformed("description", "a string");
        ctx.malformed("url", "a string");
        ctx.malformed("description", "a string");
        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].occurrences().get(), 2);
        assert_eq!(diagnostics[1].occurrences().get(), 1);
    }

    #[test_util::test]
    fn a_class_a_reader_must_act_on_keeps_every_occurrence() {
        let mut ctx = Ctx::new();
        assert_eq!(
            BreakageClass::DiscriminatorEdgeCase.aggregation(),
            super::Aggregation::PerOccurrence
        );
        for _ in 0..3 {
            ctx.report(Diagnostic::new(
                BreakageClass::DiscriminatorEdgeCase,
                Action::Degrade,
                JsonPointer::root(),
                "skipped",
            ));
        }
        assert_eq!(ctx.into_diagnostics().len(), 3);
    }

    #[test_util::test]
    fn a_habit_folds_and_a_finding_that_names_something_does_not() {
        // `unregistrable-route` carries both shapes. A refusal names the router's reason and so
        // folds — `anthropic` writes one habit into 19 templates. A collision names what it
        // collided with, which differs per route, so those stay separate records. The split falls
        // out of aggregating on the *sentence*, which is why it needs no rule of its own.
        let mut ctx = Ctx::new();
        for index in 0..4 {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                JsonPointer::root().child(index.to_string()),
                "the path is not one the router accepts",
            ));
        }
        for name in ["/a/{x}", "/a/{y}"] {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                JsonPointer::root().child(name),
                format!("`GET {name}` cannot be registered beside something else"),
            ));
        }
        let found = ctx.into_diagnostics();
        assert_eq!(found.len(), 3, "{found:#?}");
        assert_eq!(found[0].occurrences().get(), 4);
    }

    /// An explicit fold key survives the sentence changing, and the default does not.
    ///
    /// The incident this pins: the router-refusal sentence gained its `matchit` attribution, which
    /// re-keyed the aggregation and re-recorded thirteen snapshots — correct both times, but only
    /// because the change happened at every site at once. A site that folds on a stable key keeps
    /// its identity through a rewording; the first phrasing reported is the one the record keeps.
    #[test_util::test]
    fn an_explicit_fold_key_outlives_the_wording() {
        let mut ctx = Ctx::new();
        for (index, wording) in ["refused (reason A)", "refused, worded differently"]
            .iter()
            .enumerate()
        {
            ctx.report(
                Diagnostic::new(
                    BreakageClass::UnregistrableRoute,
                    Action::Degrade,
                    JsonPointer::root().child(index.to_string()),
                    *wording,
                )
                .folded_as("router-refusal"),
            );
        }
        // The same two sentences without a key are two records: the default identity is the
        // sentence, and that stays the right default for every site whose sentence is the finding.
        for (index, wording) in ["refused (reason A)", "refused, worded differently"]
            .iter()
            .enumerate()
        {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                JsonPointer::root().child(format!("unkeyed-{index}")),
                *wording,
            ));
        }
        let found = ctx.into_diagnostics();
        assert_eq!(found.len(), 3, "{found:#?}");
        assert_eq!(found[0].occurrences().get(), 2);
        assert_eq!(found[0].detail(), "refused (reason A)");
    }

    #[test_util::test]
    fn rejection_renders_its_location_when_it_has_one() {
        let error = RejectError::new(RejectKind::NonScalarKey, "a mapping key is a sequence")
            .at(JsonPointer::root().child("paths"));
        assert_eq!(
            error.to_string(),
            "non-scalar-key: a mapping key is a sequence (at /paths)"
        );
    }
}
