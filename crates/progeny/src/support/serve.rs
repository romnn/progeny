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

#![allow(
    dead_code,
    reason = "this file is shipped into generated crates; inside progeny it exists to be compiled and tested"
)]

use serde_json::Value;

/// Why a request did not match what the description says.
///
/// One enum for every way extraction can fail, so a server shapes its error bodies in one place.
/// The `Api` trait's `on_rejection` is handed this and returns whatever the service's conventions
/// call for; the default is [`RejectionResponse::of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// A parameter the description marks required, which the request did not carry.
    MissingParameter {
        operation: &'static str,
        parameter: &'static str,
    },
    /// A parameter that was present and did not parse as the type the description gives it.
    InvalidParameter {
        operation: &'static str,
        parameter: &'static str,
        detail: String,
    },
    /// A body the description marks required, which the request did not carry.
    MissingBody { operation: &'static str },
    /// A body that arrived and did not match the description.
    InvalidBody {
        operation: &'static str,
        detail: String,
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
    // `rejection.status()` is what a service overriding `on_rejection` will reach for. `allow`
    // rather than `expect` because this file is compiled again in somebody else's crate, under a
    // lint configuration that may never enable the lint being expected.
    #[allow(clippy::unused_self, reason = "public API shape, uniform answer")]
    pub fn status(&self) -> u16 {
        400
    }

    /// Which parameter or body was at fault, for a service assembling its own error body.
    #[must_use]
    pub fn operation(&self) -> &'static str {
        match self {
            Self::MissingParameter { operation, .. }
            | Self::InvalidParameter { operation, .. }
            | Self::MissingBody { operation }
            | Self::InvalidBody { operation, .. } => operation,
        }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingParameter {
                operation,
                parameter,
            } => write!(
                f,
                "`{operation}` requires `{parameter}`, which was not sent"
            ),
            Self::InvalidParameter {
                operation,
                parameter,
                detail,
            } => write!(f, "`{operation}` could not read `{parameter}`: {detail}"),
            Self::MissingBody { operation } => {
                write!(f, "`{operation}` requires a body, which was not sent")
            }
            Self::InvalidBody { operation, detail } => {
                write!(f, "`{operation}` could not read its body: {detail}")
            }
        }
    }
}

impl std::error::Error for Rejection {}

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
/// # Errors
/// When the parameter is required and absent, or present and unparsable.
pub fn read<T: serde::de::DeserializeOwned>(
    found: Result<Option<Value>, String>,
    parameter: &'static str,
    required: bool,
    operation: &'static str,
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
        Err(detail) => {
            return Err(Rejection::InvalidParameter {
                operation,
                parameter,
                detail,
            });
        }
    };
    typed(value).map_err(|source| Rejection::InvalidParameter {
        operation,
        parameter,
        detail: source,
    })
}

/// Deserialize a value that arrived as text into whatever the description says it is.
///
/// **A URL carries no types.** `?limit=3` is the three characters `l`, `i`, `m` … and the schema is
/// the only thing that says the value is a number. Handing serde the string it arrived as fails on
/// every non-string parameter in every description, which is what the example crate found the first
/// time it ran.
///
/// Tried as-is first and only then re-read, which is what keeps a genuine string a string: a
/// `type: string` parameter holding `"9"` or `"true"` succeeds on the first attempt and is never
/// reinterpreted. Only a value serde has already refused is looked at again.
///
/// # Errors
/// When the value does not match the description under either reading; the message is serde's.
pub fn typed<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    let first = match serde_json::from_value(value.clone()) {
        Ok(parsed) => return Ok(parsed),
        Err(source) => source.to_string(),
    };
    serde_json::from_value(retyped(value)).map_err(|_| first)
}

/// Every string leaf re-read as the JSON scalar it spells, where it spells one.
///
/// Deliberately not recursive into objects: an object arrives from `deepObject` or a form body,
/// where the *member* names are what the description constrains and a member's own type is asked
/// for by the same rule one level down. Arrays are, because an exploded array of integers is a list
/// of strings and every element needs the same reading.
fn retyped(value: Value) -> Value {
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            // Only a scalar. A string that happens to spell an object is a string the description
            // asked for as a string, and re-reading it would invent a shape from its contents.
            Ok(parsed @ (Value::Number(_) | Value::Bool(_))) => parsed,
            _ => Value::String(text),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(retyped).collect()),
        other => other,
    }
}

/// The same reading for a whole body assembled from text — a form or a multipart request.
///
/// # Errors
/// When the body does not match the description under either reading.
pub fn typed_body<T: serde::de::DeserializeOwned>(members: Value) -> Result<T, String> {
    let first = match serde_json::from_value(members.clone()) {
        Ok(parsed) => return Ok(parsed),
        Err(source) => source.to_string(),
    };
    let Value::Object(fields) = members else {
        return Err(first);
    };
    let retyped: serde_json::Map<String, Value> = fields
        .into_iter()
        .map(|(name, value)| (name, retyped(value)))
        .collect();
    serde_json::from_value(Value::Object(retyped)).map_err(|_| first)
}

/// A body the description marked required.
///
/// Split from [`optional_body`] rather than taking a flag, so that the handler's argument is `T`
/// where the description said required and `Option<T>` where it did not — the same distinction the
/// parameter structs carry, made by the type rather than by a check the handler has to repeat.
///
/// # Errors
/// When the body is absent, or present and unreadable.
pub fn required_body<T>(
    found: Result<Option<T>, String>,
    operation: &'static str,
) -> Result<T, Rejection> {
    match found {
        Ok(Some(body)) => Ok(body),
        Ok(None) => Err(Rejection::MissingBody { operation }),
        Err(detail) => Err(Rejection::InvalidBody { operation, detail }),
    }
}

/// A body the description did not mark required, where absent is simply absent.
///
/// # Errors
/// When the body is present and unreadable.
pub fn optional_body<T>(
    found: Result<Option<T>, String>,
    operation: &'static str,
) -> Result<Option<T>, Rejection> {
    match found {
        Ok(body) => Ok(body),
        Err(detail) => Err(Rejection::InvalidBody { operation, detail }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Rejection, RejectionResponse, optional_body, read, required_body};

    #[test]
    fn an_absent_optional_parameter_is_none_and_an_absent_required_one_is_a_rejection() {
        let absent: Result<Option<serde_json::Value>, String> = Ok(None);
        let optional: Option<String> = read(absent.clone(), "limit", false, "listPets").unwrap();
        assert_eq!(optional, None);

        let rejected = read::<String>(absent, "limit", true, "listPets").unwrap_err();
        assert_eq!(
            rejected,
            Rejection::MissingParameter {
                operation: "listPets",
                parameter: "limit",
            }
        );
    }

    #[test]
    fn a_url_carries_no_types_so_the_schema_supplies_them() {
        // The defect the example crate found on its first run: every parameter arrives as text, and
        // handing serde the text it arrived as fails for every non-string parameter in every
        // description. `?limit=3` is three characters and the schema is what says it is a number.
        let number: i64 = read(Ok(Some(json!("3"))), "limit", true, "listPets").unwrap();
        assert_eq!(number, 3);
        let flag: bool = read(Ok(Some(json!("true"))), "deep", true, "listPets").unwrap();
        assert!(flag);
        // An exploded array is a list of strings, and every element needs the same reading.
        let numbers: Vec<i64> = read(Ok(Some(json!(["1", "2"]))), "ids", true, "listPets").unwrap();
        assert_eq!(numbers, [1, 2]);
    }

    #[test]
    fn a_string_the_description_asked_for_stays_the_string_it_was() {
        // The other half, and the reason the re-reading is a *second* attempt rather than a
        // preprocessing pass: a `type: string` parameter holding `"9"` is the text `9`, and a
        // version number, an identifier or a zip code that reads as a number must survive intact.
        let text: String = read(Ok(Some(json!("9"))), "version", true, "listPets").unwrap();
        assert_eq!(text, "9");
        let text: String = read(Ok(Some(json!("true"))), "mode", true, "listPets").unwrap();
        assert_eq!(text, "true");
        let text: String = read(Ok(Some(json!("007"))), "code", true, "listPets").unwrap();
        assert_eq!(text, "007");
    }

    #[test]
    fn text_that_spells_no_scalar_is_still_refused() {
        // Coercion is not tolerance: a parameter the description types as a number and the caller
        // sent as a word is a bad request, and it has to stay one.
        assert!(read::<i64>(Ok(Some(json!("many"))), "limit", true, "listPets").is_err());
        // And a string that spells an object is a string. Re-reading it would invent a shape out
        // of the parameter's contents.
        let text: String = read(Ok(Some(json!("{\"a\":1}"))), "filter", true, "listPets").unwrap();
        assert_eq!(text, "{\"a\":1}");
    }

    #[test]
    fn a_body_assembled_from_text_gets_the_same_reading() {
        // A form body and a multipart body arrive as text member by member, exactly as a query
        // string does, so they go through the same rule rather than a second copy of it.
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Upload {
            note: String,
            rating: i32,
        }
        let parsed: Upload = super::typed_body(json!({"note": "a note", "rating": "4"})).unwrap();
        assert_eq!(
            parsed,
            Upload {
                note: "a note".to_owned(),
                rating: 4,
            }
        );
    }

    #[test]
    fn a_parameter_that_does_not_parse_says_which_one_and_why() {
        let found = Ok(Some(serde_json::json!("not a number")));
        let rejected = read::<i64>(found, "limit", true, "listPets").unwrap_err();
        let Rejection::InvalidParameter {
            parameter, detail, ..
        } = &rejected
        else {
            panic!("{rejected:?}");
        };
        assert_eq!(*parameter, "limit");
        assert!(!detail.is_empty());
        // The sentence names the operation as well, because a service logging these has no other
        // way to tell two `limit` parameters apart.
        assert!(rejected.to_string().contains("listPets"), "{rejected}");
    }

    #[test]
    fn an_optional_body_that_is_absent_is_not_a_rejection() {
        let absent: Result<Option<i32>, String> = Ok(None);
        assert_eq!(optional_body(absent.clone(), "createPet").unwrap(), None);
        assert_eq!(
            required_body(absent, "createPet").unwrap_err(),
            Rejection::MissingBody {
                operation: "createPet"
            }
        );
    }

    #[test]
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
