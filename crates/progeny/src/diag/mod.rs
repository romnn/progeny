//! Structured diagnostics: the record every deviation from the input document produces.
//!
//! Published API descriptions are broken in recurring, classifiable ways, so tolerance here
//! is a designed feature rather than accumulated patches: a closed taxonomy of actions, a
//! closed catalogue of breakage classes, and a record for every deviation. Silently wrong
//! output is the only forbidden failure mode — generating less, loudly, always beats
//! generating something plausible.

mod pointer;

use std::fmt;

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
    /// A union whose "any combination may match" semantics has no faithful Rust type.
    WildUnion,
    /// Two operations that sanitize to the same method name.
    CollidingOperationId,
    /// A (location, style, explode, shape) parameter combination OpenAPI leaves undefined.
    QuerySerializationStyle,
    /// An example payload that contradicts its own schema. Never gates generation.
    InvalidExample,
    /// A `default` value that does not typecheck against its own schema.
    InvalidDefault,
    /// A route the generated router could not register, or that collides with another.
    UnregistrableRoute,
}

impl BreakageClass {
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
            Self::WildUnion => "wild-union",
            Self::CollidingOperationId => "colliding-operation-id",
            Self::QuerySerializationStyle => "query-serialization-style",
            Self::InvalidExample => "invalid-example",
            Self::InvalidDefault => "invalid-default",
            Self::UnregistrableRoute => "unregistrable-route",
        }
    }
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
}

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
        }
    }

    /// Attach the other document locations this diagnostic is about, such as the second of
    /// two colliding operations.
    #[must_use]
    pub fn with_related(mut self, related: impl IntoIterator<Item = JsonPointer>) -> Self {
        self.related.extend(related);
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
        if !self.related.is_empty() {
            let related: Vec<String> = self.related.iter().map(ToString::to_string).collect();
            write!(f, " (see also {})", related.join(", "))?;
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
        }
    }
}

/// The error returned when a document is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl fmt::Display for RejectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.slug(), self.detail)?;
        if let Some(location) = &self.location
            && !location.is_root()
        {
            write!(f, " (at {location})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RejectError {}

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
}

impl Ctx {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The location of `token` within the node currently being read.
    pub(crate) fn child(&self, token: &str) -> JsonPointer {
        self.path.child(token)
    }

    /// Read a child node with the location extended by `token`.
    pub(crate) fn scoped<T>(&mut self, token: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(token);
        let out = f(self);
        self.path.pop();
        out
    }

    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
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

    #[test]
    fn json_line_has_a_fixed_key_order_and_escapes_detail() {
        let diagnostic = Diagnostic::new(
            BreakageClass::MalformedMember,
            Action::Degrade,
            JsonPointer::root().child("info").child("title"),
            "found \"x\"\nkept it",
        );
        assert_eq!(
            diagnostic.to_json_line(),
            r#"{"class":"malformed-member","action":"degrade","location":"/info/title","detail":"found \"x\"\nkept it"}"#
        );
    }

    #[test]
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

    #[test]
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

    #[test]
    fn control_characters_are_escaped_as_json_requires() {
        let diagnostic = Diagnostic::new(
            BreakageClass::MalformedMember,
            Action::Degrade,
            JsonPointer::root(),
            "bell\u{7}",
        );
        let line = diagnostic.to_json_line();
        // The rendering has to survive a JSON parser, or the snapshots are not JSON lines.
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["detail"], serde_json::json!("bell\u{7}"));
        assert!(line.contains("\\u0007"), "{line}");
    }

    #[test]
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

    #[test]
    fn rejection_renders_its_location_when_it_has_one() {
        let error = RejectError::new(RejectKind::NonScalarKey, "a mapping key is a sequence")
            .at(JsonPointer::root().child("paths"));
        assert_eq!(
            error.to_string(),
            "non-scalar-key: a mapping key is a sequence (at /paths)"
        );
    }
}
