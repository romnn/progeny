//! The client runtime: what every generated operation returns.
//!
//! Shipped verbatim into generated crates. Unlike [`super::style`] this cannot be compiled inside
//! progeny, because it names `reqwest` and progeny does not depend on it — so the corpus compile
//! gate is what type-checks it, with `--all-features`, on every generated crate in the tier.
//!
//! `Display` and `std::error::Error` are written out rather than derived: a derive dependency for
//! impls this stable would re-buy the macro-expansion cost the whole project is measuring.

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
    pub fn new(status: ::reqwest::StatusCode, headers: ::reqwest::header::HeaderMap, value: T) -> Self {
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
#[derive(Debug)]
pub enum Error<E> {
    /// The request never completed: DNS, TLS, connection, timeout.
    Request(::reqwest::Error),
    /// A status the document declares as an error, with its payload parsed.
    ErrorResponse(ResponseValue<E>),
    /// A status the document does not declare at all, handed back raw.
    ///
    /// Undeclared rather than unexpected in the ordinary sense: a document that lists only `200`
    /// says nothing about `503`, and inventing a shape for it would be describing a payload
    /// progeny has never seen.
    UnexpectedStatus(::reqwest::Response),
    /// The body arrived and did not match the contract the document stated.
    Decode(DecodeError),
}

impl<E> Error<E> {
    /// The status, where there was one.
    pub fn status(&self) -> Option<::reqwest::StatusCode> {
        match self {
            Self::Request(error) => error.status(),
            Self::ErrorResponse(response) => Some(response.status()),
            Self::UnexpectedStatus(response) => Some(response.status()),
            Self::Decode(_) => None,
        }
    }
}

impl<E> From<::reqwest::Error> for Error<E> {
    fn from(error: ::reqwest::Error) -> Self {
        Self::Request(error)
    }
}

impl<E> From<DecodeError> for Error<E> {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl<E> ::std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match self {
            Self::Request(error) => write!(f, "the request failed: {error}"),
            Self::ErrorResponse(response) => {
                write!(f, "the server answered {}", response.status())
            }
            Self::UnexpectedStatus(response) => write!(
                f,
                "the server answered {}, which the description does not declare",
                response.status()
            ),
            Self::Decode(error) => write!(f, "{error}"),
        }
    }
}

impl<E: ::std::fmt::Debug> ::std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn ::std::error::Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::ErrorResponse(_) | Self::UnexpectedStatus(_) => None,
        }
    }
}

/// A body that did not match the type the description said it would be.
#[derive(Debug)]
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

impl ::std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(
            f,
            "the {} response did not match the shape the description declares: {}",
            self.status, self.source
        )
    }
}

impl ::std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn ::std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A required value a builder was never given.
///
/// The runtime half of the builder interface: required setters are checked at `send()` rather than
/// encoded in the type, so this is what "you forgot one" looks like.
///
/// Named `Unset` rather than `Missing` because the buffering machinery already ships a `Missing`,
/// for an absent *member of a payload* — a different thing, in the same module.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl ::std::fmt::Display for Unset {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(
            f,
            "`{}` needs `{}`, which was never set",
            self.operation, self.what
        )
    }
}

impl ::std::error::Error for Unset {}

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

/// The same, for a declared error arm.
#[doc(hidden)]
pub async fn decode_error<T: ::serde::de::DeserializeOwned>(
    response: ::reqwest::Response,
) -> Result<ResponseValue<T>, DecodeError> {
    decode(response).await
}
