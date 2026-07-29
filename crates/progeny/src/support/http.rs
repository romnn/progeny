//! The client runtime: what every generated operation returns.
//!
//! Shipped verbatim into generated crates. It names `reqwest`, which progeny only carries as a
//! dev-dependency, so it compiles here under `cfg(test)` — same bytes tested as shipped — and again
//! on every generated crate the corpus compile gate checks, with `--all-features`.
//!
//! Error types use `thiserror` so their messages and source chains stay on the declarations they
//! describe rather than drifting into parallel implementations.

/// A successful response, with everything the caller might need about it.
///
/// Headers are exposed raw. Typed response headers are a later question; handing back what arrived
/// is the answer that cannot be wrong in the meantime.
#[derive(Debug, Clone)]
pub struct ResponseValue<T> {
    status: ::reqwest::StatusCode,
    headers: ::reqwest::header::HeaderMap,
    value: T,
}

impl<T> ResponseValue<T> {
    #[doc(hidden)]
    pub fn new(
        status: ::reqwest::StatusCode,
        headers: ::reqwest::header::HeaderMap,
        value: T,
    ) -> Self {
        Self {
            status,
            headers,
            value,
        }
    }

    /// The status the server answered with.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }

    /// The response headers, exactly as they arrived.
    pub fn headers(&self) -> &::reqwest::header::HeaderMap {
        &self.headers
    }

    /// The parsed body.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Take the parsed body, discarding the status and headers.
    pub fn into_value(self) -> T {
        self.value
    }

    /// The same response with its body put through `f`.
    ///
    /// What a generated `send` uses to wrap a body in the variant its operation's response enum
    /// gave it, without ever binding the body to a name — which matters because a `204` body is
    /// `()`, and `let value = response.into_value();` for a unit is a lint a consumer would see.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ResponseValue<U> {
        ResponseValue {
            status: self.status,
            headers: self.headers,
            value: f(self.value),
        }
    }
}

/// Why a request did not produce a declared successful response.
///
/// `E` is the operation's declared error payload, so a caller matches on a type rather than
/// re-parsing a body the document already described.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// The request never completed: DNS, TLS, connection, timeout.
    #[error("the request failed: {0}")]
    Request(#[from] ::reqwest::Error),
    /// A status the document declares as an error, with its payload parsed.
    #[error("the server answered {}", .0.status())]
    Declared(ResponseValue<E>),
    /// A status the document does not declare at all, handed back raw.
    ///
    /// Undeclared rather than unexpected in the ordinary sense: a document that lists only `200`
    /// says nothing about `503`, and inventing a shape for it would be describing a payload
    /// progeny has never seen.
    #[error(
        "the server answered {}, which the description does not declare",
        .0.status()
    )]
    UnexpectedStatus(::reqwest::Response),
    /// The body arrived and did not match the contract the document stated.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

impl<E> Error<E> {
    /// The status, where there was one.
    pub fn status(&self) -> Option<::reqwest::StatusCode> {
        match self {
            Self::Request(error) => error.status(),
            Self::Declared(response) => Some(response.status()),
            Self::UnexpectedStatus(response) => Some(response.status()),
            Self::Decode(_) => None,
        }
    }
}

/// A body that did not match the type the description said it would be.
#[derive(Debug, thiserror::Error)]
#[error("the {status} response did not match the shape the description declares: {source}")]
pub struct DecodeError {
    status: ::reqwest::StatusCode,
    source: ::serde_json::Error,
}

impl DecodeError {
    #[doc(hidden)]
    pub fn new(status: ::reqwest::StatusCode, source: ::serde_json::Error) -> Self {
        Self { status, source }
    }

    /// The status whose body failed to parse.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }
}

/// A required value a builder was never given.
///
/// The runtime half of the builder interface: required setters are checked at `send()` rather than
/// encoded in the type, so this is what "you forgot one" looks like.
///
/// Named `Unset` rather than `Missing` because the buffering machinery already ships a `Missing`,
/// for an absent *member of a payload* — a different thing, in the same module.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{operation}` needs `{what}`, which was never set")]
pub struct Unset {
    operation: &'static str,
    what: &'static str,
}

impl Unset {
    #[doc(hidden)]
    pub fn new(operation: &'static str, what: &'static str) -> Self {
        Self { operation, what }
    }
}

/// A form body whose value is not an object.
///
/// A `multipart/form-data` or form-urlencoded body names its parts after the members of an object.
/// A document that types such a body as an array or a scalar has described something with no member
/// names, and there is nothing to call the parts. Reported at `send()` because that is the only
/// place it can be: the generated type is legal Rust, and only this one call is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{operation}` sends a form body, which needs an object to name its parts from")]
pub struct NotAForm {
    operation: &'static str,
}

impl NotAForm {
    #[doc(hidden)]
    pub fn new(operation: &'static str) -> Self {
        Self { operation }
    }
}

/// A body as a `serde_json::Value`, so a form encoder can walk its members.
///
/// Returns the narrow error rather than `Error<E>`, and the caller widens it with `?`. Two things
/// follow: this is not generic over the operation's error payload, so it is compiled once per body
/// type instead of once per (body type × error type); and it does not carry `Error`'s largest
/// variant — a whole `reqwest::Response` — through a return value that can never hold one.
#[doc(hidden)]
pub fn to_value<T: ::serde::Serialize>(
    body: &T,
    operation: &'static str,
) -> ::std::result::Result<::serde_json::Value, DecodeError> {
    ::serde_json::to_value(body).map_err(|source| {
        DecodeError::new(
            ::reqwest::StatusCode::BAD_REQUEST,
            ::serde::de::Error::custom(::std::format!(
                "`{operation}` could not serialize its body: {source}"
            )),
        )
    })
}

/// Parse a successful body, or say the contract was wrong about it.
///
/// One non-generic-per-operation helper rather than an inline block per `send()`: the body of this
/// is compiled once per response type instead of once per operation.
#[doc(hidden)]
pub async fn decode<T: ::serde::de::DeserializeOwned>(
    response: ::reqwest::Response,
) -> Result<ResponseValue<T>, DecodeError> {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?;
    // An empty body deserializes as `null`, which is what `()` and `Option<T>` accept and what a
    // 204 actually sends. Without this a declared-but-empty success arm would fail to parse.
    let slice: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
    let value = ::serde_json::from_slice(slice).map_err(|error| DecodeError::new(status, error))?;
    Ok(ResponseValue::new(status, headers, value))
}
