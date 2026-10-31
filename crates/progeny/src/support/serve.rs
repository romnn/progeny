//! The serving runtime: extraction, rejection, and turning a response enum into a response.
//!
//! Shipped into generated crates behind the `server` feature. Like the client runtime this names a
//! crate progeny does not depend on — `axum` — so it is type-checked by the corpus compile gate on
//! every generated crate in the tier rather than by progeny's own build.
//!
//! Everything here is **non-generic over the operation**. A generated handler is a short function
//! that calls into these, so the work of parsing a query parameter or shaping a rejection body is
//! compiled once for the crate instead of once per operation — the same trade the style table makes,
//! and the reason the server half costs a consumer roughly what the client half does.

#![expect(
    dead_code,
    reason = "this file is shipped into generated crates; inside progeny it exists to be compiled and tested"
)]

use serde_json::Value;

/// Why a request did not match what the description says.
///
/// One enum for every way extraction can fail, so a server shapes its error bodies in one place.
/// The `Api` trait's `on_rejection` is handed this and returns whatever the service's conventions
/// call for; the default is [`RejectionResponse::of`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// A parameter the description marks required, which the request did not carry.
    #[error("`{operation}` requires `{parameter}`, which was not sent")]
    MissingParameter {
        operation: &'static str,
        parameter: &'static str,
    },
    /// A parameter that was present and did not parse as the type the description gives it.
    #[error("`{operation}` could not read `{parameter}`: {detail}")]
    InvalidParameter {
        operation: &'static str,
        parameter: &'static str,
        detail: String,
    },
    /// A body the description marks required, which the request did not carry.
    #[error("`{operation}` requires a body, which was not sent")]
    MissingBody { operation: &'static str },
    /// A body that arrived and did not match the description.
    #[error("`{operation}` could not read its body: {detail}")]
    InvalidBody {
        operation: &'static str,
        detail: String,
    },
    /// A body whose declared `Content-Type` is not the one the description admits.
    #[error("`{operation}` accepts `{declared}`, and the request declares `{sent}`")]
    UnsupportedMediaType {
        operation: &'static str,
        /// The media type the description admits at this position.
        declared: &'static str,
        /// The base media type the request declared instead.
        sent: String,
    },
}

impl Rejection {
    /// The status this rejection answers with.
    ///
    /// `400` throughout: every variant is the client having sent something the description does not
    /// describe. Deliberately not `422` for the body cases — that status is a convention some APIs
    /// keep and others do not, and picking it here would be progeny choosing a house style for
    /// somebody else's service. Overriding `on_rejection` is how a service that wants `422` gets it.
    #[must_use]
    // Every variant answers the same number today, which is the whole point of the doc comment
    // above: `422` is a house style and progeny does not have one. Kept as a method rather than the
    // associated function clippy suggests because it is public API in the generated crate, where
    // `rejection.status()` is what a service overriding `on_rejection` will reach for.
    pub fn status(&self) -> u16 {
        match self {
            Self::MissingParameter { .. }
            | Self::InvalidParameter { .. }
            | Self::MissingBody { .. }
            | Self::InvalidBody { .. } => 400,
            // The one variant with its own number: `415` *is* this refusal's status by
            // definition, not a house style.
            Self::UnsupportedMediaType { .. } => 415,
        }
    }

    /// Which parameter or body was at fault, for a service assembling its own error body.
    #[must_use]
    pub fn operation(&self) -> &'static str {
        match self {
            Self::MissingParameter { operation, .. }
            | Self::InvalidParameter { operation, .. }
            | Self::MissingBody { operation }
            | Self::InvalidBody { operation, .. }
            | Self::UnsupportedMediaType { operation, .. } => operation,
        }
    }
}

/// The default reply to a [`Rejection`].
///
/// A JSON object with a `message`, which is the shape a caller can do something with and the
/// shallowest thing that is not an empty body. It is a named type rather than a bare `Response` so
/// that overriding `on_rejection` can build one of these with a different message and still return
/// the same type.
#[derive(Debug, Clone)]
pub struct RejectionResponse {
    status: u16,
    body: Value,
}

impl RejectionResponse {
    /// The default body for a rejection: its own sentence, under `message`.
    #[must_use]
    pub fn of(rejection: &Rejection) -> Self {
        Self {
            status: rejection.status(),
            body: serde_json::json!({ "message": rejection.to_string() }),
        }
    }

    /// A reply with a status and body of the service's own choosing.
    #[must_use]
    pub fn new(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    /// The status and body, for the `IntoResponse` that ships beside this.
    #[must_use]
    pub fn into_parts(self) -> (u16, Value) {
        (self.status, self.body)
    }
}

/// Read one extracted parameter, or say why it could not be read.
///
/// `Ok(None)` from the source means the request did not carry it, which is a rejection only when
/// the description marked it required. This is the one place that rule lives, so an optional
/// parameter cannot become required by an accident in a renderer.
///
/// `reading` is the plan the description gives the parameter's text, position by position.
///
/// # Errors
/// When the parameter is required and absent, or present and unparsable.
pub fn read<T: serde::de::DeserializeOwned, E: std::fmt::Display>(
    found: Result<Option<Value>, E>,
    parameter: &'static str,
    required: bool,
    operation: &'static str,
    reading: &Reading,
) -> Result<T, Rejection> {
    let value = match found {
        Ok(Some(value)) => value,
        Ok(None) if required => {
            return Err(Rejection::MissingParameter {
                operation,
                parameter,
            });
        }
        // An absent optional parameter is `None`, which every optional field is typed to hold.
        // Asking serde for it rather than returning it directly keeps this one function usable for
        // both cases, and `T` is `Option<_>` exactly when the description said optional.
        Ok(None) => Value::Null,
        Err(source) => {
            return Err(Rejection::InvalidParameter {
                operation,
                parameter,
                detail: source.to_string(),
            });
        }
    };
    typed(value, reading).map_err(|source| Rejection::InvalidParameter {
        operation,
        parameter,
        detail: source.to_string(),
    })
}

/// A value that did not match the type declared by the description.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ServerDecodeError(#[from] serde_json::Error);

/// How a value that arrived as text is read back into the value the description types.
///
/// **A URL carries no types.**
/// `?limit=3` is the three characters `l`, `i`, `m` … and the schema is the only thing that says
/// the value is a number.
/// A form body and a multipart body carry no types either.
/// The generator knows, position by position, which text is the value itself and which is the
/// spelling of a number, a boolean or a compound, and it ships that knowledge as one of these.
///
/// A plan is per position rather than per value, so a string leaf keeps its text whatever its
/// siblings need: `label=true` stays `"true"` beside a `count=4` that becomes `4`.
/// `null` and absent members are never touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The value as it arrived.
    ///
    /// What a string leaf is, and a multipart file or JSON part, and a parameter declared with a
    /// JSON `content`, whose text the extraction already parsed.
    Keep,
    /// The JSON value a string spells, when it spells a number, a boolean, an object or an array;
    /// any other string is unchanged.
    ///
    /// An array has each element read this way, and an object that arrived as an object has its
    /// string members read this way, one level deep.
    /// This is the reading of every position whose type cannot say whether its text is a string:
    /// arbitrary JSON, and unions.
    /// It is also the reading of a compound the client wrote as one JSON text, which is parsed once
    /// and read no further, because JSON already carries its types.
    Spelled,
    /// An array whose elements are each read by the row.
    ///
    /// A lone string is one occurrence of a repeated member, which a form body cannot tell from a
    /// scalar, so it becomes a one-element array of that string read by the row.
    Items(&'static Reading),
    /// An array read position by position, with any element past the rows read as
    /// [`Self::Spelled`].
    ///
    /// A lone string is parsed once when it spells a JSON array, and is otherwise unchanged.
    Tuple(&'static [Reading]),
    /// An object whose members are each read by the row that names them, and by `others` where no
    /// row does.
    ///
    /// A lone string is parsed once when it spells a JSON object, and is otherwise unchanged.
    Object {
        members: &'static [(&'static str, Reading)],
        others: &'static Reading,
    },
    /// A compound that arrives either as one JSON text of the whole value or already rebuilt.
    ///
    /// A string is that JSON text, parsed once and read no further; a string that is not JSON is
    /// unchanged.
    /// Anything else was rebuilt from the wire, such as an array of repeated multipart parts, and
    /// reads by the row.
    Whole(&'static Reading),
}

/// Deserialize a value that arrived as text into whatever the description says it is.
///
/// One directed pass: the value is read by its plan and handed to serde once, so a string the
/// description types as a string is never reinterpreted, and a rejection carries the error serde
/// reports for the value as the plan read it.
///
/// # Errors
/// Returns [`ServerDecodeError`] when the value, read by its plan, does not match the description.
pub fn typed<T: serde::de::DeserializeOwned>(
    value: Value,
    reading: &Reading,
) -> Result<T, ServerDecodeError> {
    Ok(serde_json::from_value(read_by(value, reading))?)
}

/// One value read by its plan.
fn read_by(value: Value, reading: &Reading) -> Value {
    match (reading, value) {
        // Never touched
        (_, Value::Null) => Value::Null,
        (Reading::Keep, value) => value,
        (Reading::Spelled, value) => spelled(value),
        // Arrays
        (Reading::Items(row), Value::Array(items)) => {
            Value::Array(items.into_iter().map(|item| read_by(item, row)).collect())
        }
        (Reading::Items(row), Value::String(text)) => {
            Value::Array(vec![read_by(Value::String(text), row)])
        }
        (Reading::Tuple(rows), Value::Array(items)) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(position, item)| {
                    read_by(item, rows.get(position).unwrap_or(&Reading::Spelled))
                })
                .collect(),
        ),
        (Reading::Tuple(_), Value::String(text)) => parsed_once(text, Value::is_array),
        // Objects
        (Reading::Object { members, others }, Value::Object(fields)) => Value::Object(
            fields
                .into_iter()
                .map(|(name, field)| {
                    let row = members
                        .iter()
                        .find(|(member, _)| *member == name)
                        .map_or(*others, |(_, row)| row);
                    let field = read_by(field, row);
                    (name, field)
                })
                .collect(),
        ),
        (Reading::Object { .. }, Value::String(text)) => parsed_once(text, Value::is_object),
        // A whole compound
        (Reading::Whole(_), Value::String(text)) => {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        }
        (Reading::Whole(row), value) => read_by(value, row),
        // A shape the plan does not expect goes to serde as it arrived, and serde refuses it.
        (Reading::Items(_) | Reading::Tuple(_) | Reading::Object { .. }, other) => other,
    }
}

/// A string as the compound its JSON spells when that is the expected kind, and otherwise as it
/// arrived.
fn parsed_once(text: String, expected: fn(&Value) -> bool) -> Value {
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) if expected(&parsed) => parsed,
        _ => Value::String(text),
    }
}

/// The [`Reading::Spelled`] reading of one value.
///
/// Compounds as well as scalars, because the generated client has exactly one spelling for a
/// compound element in a query pair or a text part, and it is the element's JSON
/// (`style::scalar`).
/// `posthog` sends `properties=` with one JSON-encoded filter object per occurrence, and the
/// reader inverts precisely what the writer emits.
///
/// Not recursive into a parsed compound: the client encodes a whole element once, so one level of
/// unspelling is exactly its inverse.
/// Arrays of leaves recurse, because an exploded array of integers is a list of strings and every
/// element needs the same reading.
fn spelled(value: Value) -> Value {
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(
                parsed @ (Value::Number(_) | Value::Bool(_) | Value::Object(_) | Value::Array(_)),
            ) => parsed,
            _ => Value::String(text),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(spelled).collect()),
        // An object that arrived as an object was rebuilt from text member by member — a
        // `deepObject` or a flat `form` spelling — so its members are text to re-read, one level
        // deep as the writer spelled them.
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(name, member)| match member {
                    Value::String(_) => (name, spelled(member)),
                    other => (name, other),
                })
                .collect(),
        ),
        other => other,
    }
}

/// A body the description marked required.
///
/// Split from [`optional_body`] rather than taking a flag, so that the handler's argument is `T`
/// where the description said required and `Option<T>` where it did not — the same distinction the
/// parameter structs carry, made by the type rather than by a check the handler has to repeat.
///
/// # Errors
/// When the body is absent, or present and unreadable.
pub fn required_body<T, E: std::fmt::Display>(
    found: Result<Option<T>, E>,
    operation: &'static str,
) -> Result<T, Rejection> {
    match found {
        Ok(Some(body)) => Ok(body),
        Ok(None) => Err(Rejection::MissingBody { operation }),
        Err(source) => Err(Rejection::InvalidBody {
            operation,
            detail: source.to_string(),
        }),
    }
}

/// A body the description did not mark required, where absent is simply absent.
///
/// # Errors
/// When the body is present and unreadable.
pub fn optional_body<T, E: std::fmt::Display>(
    found: Result<Option<T>, E>,
    operation: &'static str,
) -> Result<Option<T>, Rejection> {
    match found {
        Ok(body) => Ok(body),
        Err(source) => Err(Rejection::InvalidBody {
            operation,
            detail: source.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::json;

    use super::{Reading, Rejection, RejectionResponse, optional_body, read, required_body, typed};

    /// The reader inverts exactly what the writer emits, one compound level deep.
    ///
    /// The wire probe's `posthog` find: the client's one spelling for a compound element in a query
    /// pair is the element's JSON, and the reader refused it — `properties=` carried a JSON-encoded
    /// filter per occurrence and serde was handed the text where the struct belonged. The guard in
    /// the other direction matters as much: a value the description types as a *string* keeps its
    /// braces, because its plan keeps it.
    #[test_util::test]
    fn a_string_spelling_a_compound_is_reread_where_a_compound_is_expected() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Filter {
            key: String,
        }
        let arrived = json!(["{\"key\":\"a\"}"]);
        let parsed: Vec<Filter> = typed(arrived, &Reading::Items(&Reading::Spelled))?;
        assert_eq!(
            parsed,
            vec![Filter {
                key: "a".to_owned()
            }]
        );

        let stays_text: String = typed(json!("{\"key\":\"a\"}"), &Reading::Keep)?;
        assert_eq!(stays_text, "{\"key\":\"a\"}");
    }

    #[test_util::test]
    fn an_absent_optional_parameter_is_none_and_an_absent_required_one_is_a_rejection() {
        let absent = Ok::<_, Infallible>(None);
        let optional: Option<String> =
            read(absent.clone(), "limit", false, "listPets", &Reading::Keep)?;
        assert_eq!(optional, None);

        let rejected = read::<String, _>(absent, "limit", true, "listPets", &Reading::Keep)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert_eq!(
            rejected,
            Rejection::MissingParameter {
                operation: "listPets",
                parameter: "limit",
            }
        );
    }

    #[test_util::test]
    fn a_url_carries_no_types_so_the_schema_supplies_them() {
        // The defect the example crate found on its first run: every parameter arrives as text, and
        // handing serde the text it arrived as fails for every non-string parameter in every
        // description. `?limit=3` is three characters and the schema is what says it is a number.
        let number: i64 = read(
            Ok::<_, Infallible>(Some(json!("3"))),
            "limit",
            true,
            "listPets",
            &Reading::Spelled,
        )?;
        assert_eq!(number, 3);
        let flag: bool = read(
            Ok::<_, Infallible>(Some(json!("true"))),
            "deep",
            true,
            "listPets",
            &Reading::Spelled,
        )?;
        assert!(flag);
        // An exploded array is a list of strings, and every element needs the same reading.
        let numbers: Vec<i64> = read(
            Ok::<_, Infallible>(Some(json!(["1", "2"]))),
            "ids",
            true,
            "listPets",
            &Reading::Items(&Reading::Spelled),
        )?;
        assert_eq!(numbers, [1, 2]);
    }

    #[test_util::test]
    fn a_string_the_description_asked_for_stays_the_string_it_was() {
        // The other half: a `type: string` parameter holding `"9"` is the text `9`, and a version
        // number, an identifier or a zip code that reads as a number must survive intact.
        for text in ["9", "true", "007"] {
            let kept: String = read(
                Ok::<_, Infallible>(Some(json!(text))),
                "version",
                true,
                "listPets",
                &Reading::Keep,
            )?;
            assert_eq!(kept, text);
        }
    }

    #[test_util::test]
    fn text_that_spells_no_scalar_is_still_refused() {
        // Coercion is not tolerance: a parameter the description types as a number and the caller
        // sent as a word is a bad request, and it has to stay one.
        assert!(
            read::<i64, _>(
                Ok::<_, Infallible>(Some(json!("many"))),
                "limit",
                true,
                "listPets",
                &Reading::Spelled,
            )
            .is_err()
        );
        // And a string that spells an object is a string. Re-reading it would invent a shape out
        // of the parameter's contents.
        let text: String = read(
            Ok::<_, Infallible>(Some(json!("{\"a\":1}"))),
            "filter",
            true,
            "listPets",
            &Reading::Keep,
        )?;
        assert_eq!(text, "{\"a\":1}");
    }

    #[test_util::test]
    fn a_body_assembled_from_text_gets_the_same_reading() {
        // A form body and a multipart body arrive as text member by member, exactly as a query
        // string does, so they go through the same rule rather than a second copy of it.
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Upload {
            note: String,
            rating: i32,
        }
        let reading = Reading::Object {
            members: &[("note", Reading::Keep), ("rating", Reading::Spelled)],
            others: &Reading::Spelled,
        };
        let parsed: Upload = typed(json!({"note": "a note", "rating": "4"}), &reading)?;
        assert_eq!(
            parsed,
            Upload {
                note: "a note".to_owned(),
                rating: 4,
            }
        );
    }

    /// A string member keeps its text while the others are re-read, so a string that spells a
    /// number or a boolean does not fail beside a member that needed re-reading.
    #[test_util::test]
    fn a_string_member_keeps_its_text_while_the_others_are_re_read() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Login {
            label: String,
            count: i64,
        }
        let reading = Reading::Object {
            members: &[("label", Reading::Keep), ("count", Reading::Spelled)],
            others: &Reading::Spelled,
        };
        let parsed: Login = typed(json!({"label": "true", "count": "4"}), &reading)?;
        assert_eq!(
            parsed,
            Login {
                label: "true".to_owned(),
                count: 4,
            }
        );
    }

    /// A member no row names is read by the object's `others` row, which is how a member captured
    /// by `additionalProperties` gets the reading of the capture's value type.
    #[test_util::test]
    fn a_captured_member_is_read_by_the_capture_row() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Labels {
            count: i64,
            #[serde(flatten)]
            extra: BTreeMap<String, String>,
        }
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Counts {
            count: i64,
            #[serde(flatten)]
            extra: BTreeMap<String, i64>,
        }
        let arrived = json!({"count": "4", "extra": "true", "other": "7"});

        // Captured strings keep their text beside a declared member that needed re-reading.
        let labels: Labels = typed(
            arrived.clone(),
            &Reading::Object {
                members: &[("count", Reading::Spelled)],
                others: &Reading::Keep,
            },
        )?;
        assert_eq!(labels.count, 4);
        assert_eq!(
            labels.extra,
            BTreeMap::from([
                ("extra".to_owned(), "true".to_owned()),
                ("other".to_owned(), "7".to_owned()),
            ])
        );

        // Captured integers are still read as the numbers they spell.
        let counts: Counts = typed(
            json!({"count": "4", "other": "7"}),
            &Reading::Object {
                members: &[("count", Reading::Spelled)],
                others: &Reading::Spelled,
            },
        )?;
        assert_eq!(counts.extra, BTreeMap::from([("other".to_owned(), 7)]));
    }

    /// A tuple is read position by position, so a string position keeps its text beside an
    /// integer position that needs re-reading.
    #[test_util::test]
    fn a_tuple_is_read_position_by_position() {
        let pair: (String, i64) = typed(
            json!(["9", "4"]),
            &Reading::Tuple(&[Reading::Keep, Reading::Spelled]),
        )?;
        assert_eq!(pair, ("9".to_owned(), 4));
        // A tuple written as one JSON text is parsed once and not read any further.
        let pair: (String, i64) = typed(
            json!("[\"9\",4]"),
            &Reading::Tuple(&[Reading::Keep, Reading::Spelled]),
        )?;
        assert_eq!(pair, ("9".to_owned(), 4));
    }

    /// A lone string at an array position is one occurrence of a repeated member, read by the
    /// element row rather than parsed, so an element that happens to spell JSON stays text.
    #[test_util::test]
    fn a_lone_string_at_an_array_position_is_one_element() {
        let one: Vec<String> = typed(json!("[]"), &Reading::Items(&Reading::Keep))?;
        assert_eq!(one, ["[]"]);
        let one: Vec<i64> = typed(json!("4"), &Reading::Items(&Reading::Spelled))?;
        assert_eq!(one, [4]);
    }

    /// A value the plan keeps is handed to serde as it arrived: a JSON-content parameter's
    /// document was parsed by the extraction, and its strings are strings.
    #[test_util::test]
    fn a_kept_value_is_not_re_read() {
        let document: serde_json::Value = read(
            Ok::<_, Infallible>(Some(json!({"payload": {"labels": ["1"]}}))),
            "filter",
            true,
            "listPets",
            &Reading::Keep,
        )?;
        assert_eq!(document, json!({"payload": {"labels": ["1"]}}));
        let text: serde_json::Value = read(
            Ok::<_, Infallible>(Some(json!("1"))),
            "filter",
            true,
            "listPets",
            &Reading::Keep,
        )?;
        assert_eq!(text, json!("1"));
    }

    /// An object parameter's members are read one by one, and its string members keep their
    /// text.
    #[test_util::test]
    fn an_object_parameter_is_read_member_by_member() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Page {
            number: i64,
            cursor: String,
        }
        let reading = Reading::Object {
            members: &[("number", Reading::Spelled), ("cursor", Reading::Keep)],
            others: &Reading::Spelled,
        };
        let page: Page = typed(json!({"number": "2", "cursor": "20"}), &reading)?;
        assert_eq!(
            page,
            Page {
                number: 2,
                cursor: "20".to_owned(),
            }
        );
    }

    #[test_util::test]
    fn a_parameter_that_does_not_parse_says_which_one_and_why() {
        let found = Ok::<_, Infallible>(Some(serde_json::json!("not a number")));
        let rejected = read::<i64, _>(found, "limit", true, "listPets", &Reading::Spelled)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        let Rejection::InvalidParameter {
            parameter, detail, ..
        } = &rejected
        else {
            panic!("{rejected:?}");
        };
        assert_eq!(*parameter, "limit");
        assert_ne!(detail, "");
        // The sentence names the operation as well, because a service logging these has no other
        // way to tell two `limit` parameters apart.
        assert!(rejected.to_string().contains("listPets"), "{rejected}");
    }

    /// A rejection carries serde's error for the value as its plan read it.
    ///
    /// `count` arrives as text the plan re-reads into a number, so the error is the one about
    /// `label`, a number where the description types a string.
    #[test_util::test]
    fn a_rejection_carries_the_error_for_the_value_as_planned() {
        #[derive(Debug, serde::Deserialize)]
        #[expect(dead_code, reason = "the test only checks that deserializing fails")]
        struct Login {
            label: String,
            count: i64,
        }
        let reading = Reading::Object {
            members: &[("label", Reading::Keep), ("count", Reading::Spelled)],
            others: &Reading::Spelled,
        };
        let rejected = typed::<Login>(json!({"label": 4, "count": "4"}), &reading)
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert!(
            rejected.to_string().contains("expected a string"),
            "{rejected}"
        );
    }

    #[test_util::test]
    fn an_optional_body_that_is_absent_is_not_a_rejection() {
        let absent: Result<Option<i32>, Infallible> = Ok(None);
        assert_eq!(optional_body(absent, "createPet")?, None);
        assert_eq!(
            required_body(absent, "createPet")
                .err()
                .ok_or_eyre("the test expects this operation to fail")?,
            Rejection::MissingBody {
                operation: "createPet"
            }
        );
    }

    #[test_util::test]
    fn the_default_rejection_body_says_what_went_wrong() {
        let rejection = Rejection::MissingParameter {
            operation: "listPets",
            parameter: "limit",
        };
        let response = RejectionResponse::of(&rejection);
        assert_eq!(response.status, 400);
        assert_eq!(
            response.body["message"],
            serde_json::json!("`listPets` requires `limit`, which was not sent")
        );
    }
}
