//! Checked schema overrides applied before reference resolution and shape lowering.

use crate::config::Config;
use crate::diag::{JsonPointer, RejectError, RejectKind};
use crate::doc::ParsedDocument;
use crate::schema::{OneOrMany, Schema, TypeName};

/// Applies every configured nullability override to the typed schema model.
pub(crate) fn apply(parsed: &mut ParsedDocument, config: &Config) -> Result<(), RejectError> {
    for (written, expected) in &config.nullability_overrides {
        let Some(pointer) = JsonPointer::parse(written) else {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the configuration's `nullability-overrides` key `{written}` is not an RFC \
                     6901 JSON Pointer"
                ),
            ));
        };
        if pointer.is_root() {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                "the configuration's `nullability-overrides` cannot target the document root",
            ));
        }
        let Some(schema) = parsed.schemas.at_mut(&pointer) else {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the configuration's `nullability-overrides` names `{written}`, but the \
                     document declares no schema at that pointer"
                ),
            )
            .at(pointer));
        };
        let Schema::Object(schema) = schema else {
            return Err(changed_shape(
                &pointer,
                expected.as_str(),
                "a boolean schema",
            ));
        };
        let Some(declared) = schema.types.as_ref() else {
            return Err(changed_shape(
                &pointer,
                expected.as_str(),
                "a schema without one declared `type`",
            ));
        };
        if declared.iter().any(|ty| *ty == TypeName::Null) {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the nullability override expects `{}` at `{written}`, but that schema is \
                     already nullable; remove the dead override",
                    expected.as_str()
                ),
            )
            .at(pointer));
        }
        let mut types = declared.iter();
        let matches = types
            .next()
            .is_some_and(|ty| ty.as_str() == expected.as_str())
            && types.next().is_none();
        if !matches {
            let found = declared
                .iter()
                .map(TypeName::as_str)
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(changed_shape(
                &pointer,
                expected.as_str(),
                &format!("declared type `{found}`"),
            ));
        }
        schema.types = Some(OneOrMany::Many(vec![
            TypeName::parse(expected.as_str()),
            TypeName::Null,
        ]));
    }
    Ok(())
}

fn changed_shape(pointer: &JsonPointer, expected: &str, found: &str) -> RejectError {
    RejectError::new(
        RejectKind::UnsatisfiableConfig,
        format!(
            "the nullability override pins `{expected}` at `{pointer}`, but the document now has \
             {found} there"
        ),
    )
    .at(pointer.clone())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use color_eyre::eyre::{self, OptionExt as _};

    use crate::{Config, RejectError, RejectKind, SchemaType, generate};

    const PROPERTY: &str = "/components/schemas/Patch/properties/nickname";

    fn document(property: &str) -> Vec<u8> {
        r#"{"openapi":"3.1.0","paths":{},"components":{"schemas":{"Patch":{"type":"object","properties":{"nickname":PROPERTY}}}}}"#
            .replace("PROPERTY", property)
            .into_bytes()
    }

    fn config(pointer: &str, expected: SchemaType) -> Config {
        Config {
            nullability_overrides: BTreeMap::from([(pointer.to_owned(), expected)]),
            ..Config::default()
        }
    }

    fn rejected(input: &[u8], config: &Config) -> eyre::Result<RejectError> {
        generate(input, config)
            .err()
            .ok_or_eyre("the override should reject generation")
    }

    #[test_util::test]
    fn a_missing_override_path_is_a_hard_error() {
        let missing = "/components/schemas/Patch/properties/missing";
        let error = rejected(
            &document(r#"{"type":"string"}"#),
            &config(missing, SchemaType::String),
        )?;
        assert_eq!(error.kind(), RejectKind::UnsatisfiableConfig);
        assert_eq!(
            error.location().map(ToString::to_string).as_deref(),
            Some(missing)
        );
        assert!(error.detail().contains("declares no schema"), "{error}");
    }

    #[test_util::test]
    fn a_changed_declared_type_is_a_hard_error() {
        let error = rejected(
            &document(r#"{"type":"integer"}"#),
            &config(PROPERTY, SchemaType::String),
        )?;
        assert_eq!(error.kind(), RejectKind::UnsatisfiableConfig);
        assert!(error.detail().contains("pins `string`"), "{error}");
        assert!(
            error.detail().contains("declared type `integer`"),
            "{error}"
        );
    }

    #[test_util::test]
    fn an_already_nullable_override_is_a_hard_error() {
        let error = rejected(
            &document(r#"{"type":["string","null"]}"#),
            &config(PROPERTY, SchemaType::String),
        )?;
        assert_eq!(error.kind(), RejectKind::UnsatisfiableConfig);
        assert!(error.detail().contains("already nullable"), "{error}");
        assert!(
            error.detail().contains("remove the dead override"),
            "{error}"
        );
    }

    #[test_util::test]
    fn a_non_pointer_override_key_is_a_hard_error() {
        let error = rejected(
            &document(r#"{"type":"string"}"#),
            &config("Patch.nickname", SchemaType::String),
        )?;
        assert_eq!(error.kind(), RejectKind::UnsatisfiableConfig);
        assert!(error.detail().contains("RFC 6901 JSON Pointer"), "{error}");
    }
}
