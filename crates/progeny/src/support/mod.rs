//! Static source shipped into generated crates.
//!
//! Emitted from a versioned file in the progeny tree rather than token-generated, because it does
//! not vary per document: generating it would be complexity with no input. The same file is compiled
//! as part of progeny, so it is type-checked and unit-tested here before it is shipped anywhere.
//!
//! In a generated crate it is `#[doc(hidden)]` and never part of that crate's public API.

mod buffered;

/// The buffering machinery the hand-written `Deserialize` implementations call into.
const BUFFERED: &str = include_str!("buffered.rs");

/// The support source as tokens, so it composes with the rendered items.
///
/// Parsed rather than pasted so that a syntax error in the shipped source is a progeny test failure
/// rather than a consumer's compile error. A file that does not parse falls back to being emitted
/// verbatim, which is what a caller would want anyway — but the module's own tests compile it, so
/// that cannot happen unnoticed.
pub(crate) fn tokens() -> proc_macro2::TokenStream {
    match syn::parse_file(BUFFERED) {
        Ok(file) => quote::quote! { #file },
        Err(_) => BUFFERED.parse().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use serde::de::{Deserialize, Deserializer};

    use super::buffered::{
        Assemble, Buffer, BufferVisitor, Content, ContentDeserializer, Missing, Unknown,
    };

    /// The shape the spike hand-writes: a required member, an optional one, and one whose wire name
    /// is not its Rust name.
    #[derive(Debug, PartialEq)]
    struct Spike {
        required: String,
        optional: Option<i64>,
        renamed: bool,
    }

    /// Written by hand here in exactly the form the renderer emits, so that the machinery is tested
    /// through the same shape the generated code uses.
    impl<'de> Assemble<'de> for Spike {
        const NAME: &'static str = "Spike";
        const FIELDS: &'static [&'static str] = &["required", "optional", "wireName"];
        const DEFAULTED: &'static [bool] = &[false, false, false];

        fn assemble<E>(buffer: &mut Buffer<'de>) -> Result<Self, E>
        where
            E: serde::de::Error,
        {
            Ok(Self {
                required: buffer.take("required")?,
                optional: buffer.take("optional")?,
                renamed: buffer.take("wireName")?,
            })
        }
    }

    impl<'de> Deserialize<'de> for Spike {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_struct(
                Self::NAME,
                Self::FIELDS,
                BufferVisitor::<Self>::new(Unknown::Ignore),
            )
        }
    }

    fn parse(text: &str) -> Result<Spike, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test]
    fn every_member_is_read_by_its_wire_name() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": 1, "wireName": true}"#).unwrap(),
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
    }

    #[test]
    fn a_bare_option_accepts_an_absent_member() {
        // No `default` attribute anywhere: serde routes a missing member through a deserializer
        // whose `deserialize_option` answers `visit_none`, and this mirrors that.
        assert_eq!(
            parse(r#"{"required": "a", "wireName": false}"#)
                .unwrap()
                .optional,
            None
        );
    }

    #[test]
    fn an_explicit_null_and_an_absent_member_agree() {
        assert_eq!(
            parse(r#"{"required": "a", "optional": null, "wireName": false}"#)
                .unwrap()
                .optional,
            None
        );
    }

    #[test]
    fn a_duplicate_member_is_rejected_rather_than_last_write_wins() {
        // The case a map-based buffer would lose silently, which is why the buffer is a list.
        let error = parse(r#"{"required": "a", "required": "b", "wireName": false}"#).unwrap_err();
        assert!(
            error.to_string().starts_with("duplicate field `required`"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_member_says_which_one() {
        let error = parse(r#"{"wireName": false}"#).unwrap_err();
        assert!(
            error.to_string().starts_with("missing field `required`"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_member_is_ignored_under_that_policy() {
        assert!(
            parse(r#"{"required": "a", "wireName": false, "surprise": {"deep": [1]}}"#).is_ok()
        );
    }

    #[test]
    fn an_unknown_member_is_refused_under_the_other_one() {
        let mut deserializer =
            serde_json::Deserializer::from_str(r#"{"required": "a", "surprise": 1}"#);
        let error = deserializer
            .deserialize_struct(
                <Spike as Assemble>::NAME,
                <Spike as Assemble>::FIELDS,
                BufferVisitor::<Spike>::new(Unknown::Deny),
            )
            .expect_err("an undeclared member should be refused");
        assert!(
            error.to_string().starts_with("unknown field `surprise`"),
            "{error}"
        );
    }

    #[test]
    fn a_struct_can_be_read_from_a_sequence_the_way_the_derive_reads_one() {
        assert_eq!(
            serde_json::from_str::<Spike>(r#"["a", 1, true]"#).unwrap(),
            Spike {
                required: "a".to_owned(),
                optional: Some(1),
                renamed: true,
            }
        );
        let error = serde_json::from_str::<Spike>(r#"["a"]"#).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid length 1, expected struct Spike with 3 elements"),
            "{error}"
        );
    }

    #[test]
    fn a_member_of_the_wrong_type_reports_it_the_way_the_format_does() {
        let error = parse(r#"{"required": 7, "wireName": false}"#).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid type: integer `7`, expected a string"),
            "{error}"
        );
    }

    #[test]
    fn a_buffered_value_replays_into_any_type() {
        // The machinery has to be able to hand a buffered value to an arbitrary `Deserialize`, or
        // a struct could only hold scalars.
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2)]);
        let replayed: Vec<u64> =
            Vec::deserialize(ContentDeserializer::<serde_json::Error>::new(content)).unwrap();
        assert_eq!(replayed, [1, 2]);

        let content = Content::Map(vec![(
            Content::Str("a"),
            Content::Seq(vec![Content::Bool(true)]),
        )]);
        let replayed: std::collections::BTreeMap<String, Vec<bool>> =
            Deserialize::deserialize(ContentDeserializer::<serde_json::Error>::new(content))
                .unwrap();
        assert_eq!(replayed["a"], [true]);
    }

    #[test]
    fn a_missing_member_of_a_non_optional_type_is_an_error_and_not_a_default() {
        let outcome: Result<i64, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert!(outcome.is_err());
        let outcome: Result<Option<i64>, serde_json::Error> =
            Deserialize::deserialize(Missing::new("count"));
        assert_eq!(outcome.unwrap(), None);
    }
}
