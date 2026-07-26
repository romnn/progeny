//! The typed customization set.
//!
//! Everything a caller can say, as a closed set of typed values. There is no string-typed
//! passthrough field anywhere in it, which is what makes an unsupported customization a config
//! parse error rather than a latent codegen bug: the opaque-token injection path that let a
//! caller change a wire contract behind the generator's back does not exist here.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::diag::{Action, BreakageClass, Diagnostic};

/// How progeny should generate, and what the caller refuses to accept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Which diagnostics the caller treats as build failures.
    #[serde(default)]
    pub deny: Deny,
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
