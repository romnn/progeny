//! The typed customization set.
//!
//! Everything a caller can say, as a closed set of typed values. There is no string-typed
//! passthrough field anywhere in it, which is what makes an unsupported customization a config
//! parse error rather than a latent codegen bug: the opaque-token injection path that let a
//! caller change a wire contract behind the generator's back does not exist here.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::diag::{Action, BreakageClass, Diagnostic};

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
    #[serde(default)]
    pub unknown_fields: UnknownFields,
    /// The same, for named types.
    #[serde(default)]
    pub type_unknown_fields: BTreeMap<String, UnknownFields>,
    /// Names to use instead of the ones progeny derives, keyed by component name or by the
    /// schema's JSON Pointer.
    ///
    /// An explicit name is a declaration of identity: two shapes named here never merge with each
    /// other, even when they are structurally identical.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// Which `Deserialize`/`Serialize` implementation strategy to use.
    #[serde(default)]
    pub serde_impl: SerdeImpl,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

/// How to package the generated source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Packaging {
    /// A complete crate: a manifest and a `src` tree.
    #[default]
    Crate,
    /// A workspace containing `<name>-types`, `<name>-client`, and `<name>-server` crates.
    ///
    /// The types crate has no features; the edge crates pin its exact generated version and may be
    /// published after it.
    Workspace,
    /// One file to `include!` from a build script or to check in.
    Module,
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

impl Config {
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
    pub fn denied<'a>(
        &'a self,
        diagnostics: &'a [Diagnostic],
    ) -> impl Iterator<Item = &'a Diagnostic> {
        diagnostics.iter().filter(|diagnostic| {
            self.deny.actions.contains(&diagnostic.action())
                || self.deny.classes.contains(&diagnostic.class())
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
}
