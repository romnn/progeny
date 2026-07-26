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
    /// Whether to emit a crate or a module tree.
    #[serde(default)]
    pub packaging: Packaging,
    /// What to call the emitted crate, when emitting one.
    #[serde(default)]
    pub package: Package,
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
    /// `contentEncoding: base64` and binary payloads.
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
    /// The derive, always. The escape hatch, and it has to stay reachable: buffering requires a
    /// self-describing format, and a caller may feed generated types to one that is not.
    #[default]
    DeriveAlways,
    /// The hand-written implementation wherever the eligibility function allows it.
    HandWrittenWhereEligible,
}

/// Whether to emit a crate or a module tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Packaging {
    /// A complete crate: a manifest and a `src` tree.
    #[default]
    Crate,
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
    /// Both spellings are accepted because both are how a caller thinks about a type: `Pet` for a
    /// component, and the pointer for a shape the document never named.
    fn keyed<'a, T>(
        table: &'a BTreeMap<String, T>,
        component: Option<&str>,
        address: &str,
    ) -> Option<&'a T> {
        component
            .and_then(|name| table.get(name))
            .or_else(|| table.get(address))
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

    fn diagnostic(class: BreakageClass, action: Action) -> Diagnostic {
        Diagnostic::new(class, action, JsonPointer::root(), "detail")
    }

    #[test]
    fn the_default_configuration_denies_nothing() {
        let config = Config::default();
        let diagnostics = [
            diagnostic(BreakageClass::WildUnion, Action::Degrade),
            diagnostic(BreakageClass::InvalidExample, Action::Warn),
        ];
        assert_eq!(config.denied(&diagnostics).count(), 0);
    }

    #[test]
    fn a_policy_is_read_from_the_names_diagnostics_render_with() {
        let config: Config = toml::from_str(
            r#"
            [deny]
            actions = ["degrade"]
            classes = ["dangling-ref"]
            "#,
        )
        .unwrap();
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

    #[test]
    fn an_unsupported_knob_is_a_config_error_not_a_silent_no_op() {
        let error = toml::from_str::<Config>("attribute = \"#[serde(skip)]\"").unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn an_unknown_diagnostic_name_is_a_config_error() {
        assert!(toml::from_str::<Config>("[deny]\nactions = [\"reject\"]").is_err());
        assert!(toml::from_str::<Config>("[deny]\nclasses = [\"whatever\"]").is_err());
    }
}
