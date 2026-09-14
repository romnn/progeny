//! The client runtime: what every generated operation returns.
//!
//! Shipped verbatim into generated crates. It names `reqwest`, which progeny only carries as a
//! dev-dependency, so it compiles here under `cfg(test)` — same bytes tested as shipped — and again
//! on every generated crate the corpus compile gate checks, with `--all-features`.
//!
//! Error types use `thiserror` so their messages and source chains stay on the declarations they
//! describe rather than drifting into parallel implementations.

use super::{Decoded, Degradations};
// Only the lenient decoder names these two, and strict decoding leaves the decoder — and this
// import beside it — out of the generated crate.
use super::{Lenient, Site};

/// A successful response, with everything the caller might need about it.
///
/// Headers are exposed raw. Typed response headers are a later question; handing back what arrived
/// is the answer that cannot be wrong in the meantime.
///
/// The body arrives with its [`Degradations`]: everything the decoder tolerated on the way to
/// the value, keyed by the place in the description it happened at. Empty when the payload
/// matched the description exactly, and always empty for a strictly decoded response.
#[derive(Debug, Clone)]
pub struct ResponseValue<T> {
    status: ::reqwest::StatusCode,
    headers: ::reqwest::header::HeaderMap,
    value: T,
    degradations: Degradations,
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
            degradations: Degradations::new(),
        }
    }

    #[doc(hidden)]
    pub fn decoded(
        status: ::reqwest::StatusCode,
        headers: ::reqwest::header::HeaderMap,
        decoded: Decoded<T>,
    ) -> Self {
        Self {
            status,
            headers,
            value: decoded.value,
            degradations: decoded.degradations,
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

    /// Take the parsed body, discarding the status, headers and degradations.
    pub fn into_value(self) -> T {
        self.value
    }

    /// What the decoder tolerated to produce the body.
    ///
    /// Empty for a payload that matched the description exactly. Each entry names the generated
    /// type and member the drift was seen at, what was tolerated, how often across the payload,
    /// and the pointer in the description an override would use.
    pub fn degradations(&self) -> &Degradations {
        &self.degradations
    }

    /// Whether the decoder tolerated anything on the way to the body.
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }

    /// Take the parsed body with its degradations, discarding the status and headers.
    pub fn into_decoded(self) -> Decoded<T> {
        Decoded {
            value: self.value,
            degradations: self.degradations,
        }
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
            degradations: self.degradations,
        }
    }
}

/// A response the decoder tolerated something in, as handed to a client's observer.
///
/// Borrowed, because the observer sees every degraded response and most observers count or log;
/// one that keeps a report clones it.
#[derive(Debug, Clone, Copy)]
pub struct Degraded<'a> {
    /// The generated method the response answered.
    pub operation: &'static str,
    /// The status the server answered with.
    pub status: ::reqwest::StatusCode,
    /// What the decoder tolerated.
    pub degradations: &'a Degradations,
}

/// A hook a client calls with every degraded response, before handing the response back.
///
/// The one way to see drift across a whole application without threading each response's
/// report through every call site: counting into a metric, logging at a level the application
/// chooses, or failing a test. It sees the same report the response carries and can change
/// nothing about it. `Send + Sync` so a client holding one can be shared across tasks.
#[derive(Clone)]
pub struct Observer(
    ::std::sync::Arc<dyn Fn(Degraded<'_>) + ::std::marker::Send + ::std::marker::Sync>,
);

impl Observer {
    /// Wrap a function as an observer.
    pub fn new(
        observe: impl Fn(Degraded<'_>) + ::std::marker::Send + ::std::marker::Sync + 'static,
    ) -> Self {
        Self(::std::sync::Arc::new(observe))
    }

    /// Hand one degraded response to the function.
    pub fn notify(&self, degraded: Degraded<'_>) {
        (self.0)(degraded);
    }
}

impl ::std::fmt::Debug for Observer {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Observer")
    }
}

/// Why a request did not produce a declared successful response.
///
/// `E` is the operation's declared error payload, so a caller matches on a type rather than
/// re-parsing a body the document already described.
///
/// The two variants that carry a whole response are boxed. This type is the `Err` half of every
/// operation's return, so its width is paid on the success path too — and unboxed, a `HeaderMap`
/// alone puts it past the width at which `clippy::result_large_err` calls a `Result` too expensive
/// to return by value, in the generated crate and in its consumer's build alike.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// The request never completed: DNS, TLS, connection, timeout.
    #[error("the request failed: {0}")]
    Request(#[from] ::reqwest::Error),
    /// A status the document declares as an error, with its payload parsed.
    #[error("the server answered {}", .0.status())]
    Declared(::std::boxed::Box<ResponseValue<E>>),
    /// A status the document does not declare at all, handed back raw.
    ///
    /// Undeclared rather than unexpected in the ordinary sense: a document that lists only `200`
    /// says nothing about `503`, and inventing a shape for it would be describing a payload
    /// progeny has never seen.
    #[error(
        "the server answered {}, which the description does not declare",
        .0.status()
    )]
    UnexpectedStatus(::std::boxed::Box<::reqwest::Response>),
    /// The body arrived and did not match the contract the document stated.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// A page of a stream was degraded on the path to its items or its next cursor.
    ///
    /// A stream has no way to hand a page's report back beside the items, and a member on the
    /// path it walks that was absent, unreadable, or a list with an element skipped would either
    /// end the stream early or drop items without a word. So it stops with the report instead;
    /// the plain `send` on the same request returns the page with its report for a caller who
    /// wants what could be read.
    /// Drift inside an item — a member of one record absent or unreadable — is not on the path
    /// and does not stop the stream; the item arrives as its read form says.
    ///
    /// Only a leniently decoded client can produce this: a strict decode fails the page as a
    /// [`Error::Decode`] before there is a report to look at.
    #[error("a page of the stream was degraded on the path to its items or next cursor: {0}")]
    DegradedPage(::std::boxed::Box<Degradations>),
    /// A path parameter whose rendered segment is `.` or `..`.
    ///
    /// Every WHATWG URL parser — reqwest's included — folds dot segments away before the
    /// request leaves, so `/files/{id}/metadata` with `id = ".."` would silently ask for
    /// `/metadata`: another endpoint entirely. Percent-encoding cannot save the spelling
    /// (`%2E` segments normalize the same way), so the request is refused before it is built.
    #[error(
        "path parameter `{parameter}` renders the segment `{rendered}`, which URL \
         normalization would fold into another endpoint's path"
    )]
    UnsendablePath {
        /// The first path parameter of the offending segment.
        parameter: &'static str,
        /// The segment as it would have gone on the wire.
        rendered: String,
    },
}

impl<E> Error<E> {
    /// The status, where there was one.
    pub fn status(&self) -> Option<::reqwest::StatusCode> {
        match self {
            Self::Request(error) => error.status(),
            Self::Declared(response) => Some(response.status()),
            Self::UnexpectedStatus(response) => Some(response.status()),
            Self::Decode(_) | Self::DegradedPage(_) | Self::UnsendablePath { .. } => None,
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
/// type instead of once per (body type × error type); and its `Err` names the one failure
/// serializing a body can produce rather than variants this call can never reach.
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
pub async fn decode_json<T: ::serde::de::DeserializeOwned>(
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

/// Read a successful body leniently: as far as the payload allows, with everything it tolerated
/// on the way recorded beside the value.
///
/// Compiled once per response type, like [`decode_json`]. A root that is not what the
/// description declares — an object where a list was promised — is still a [`DecodeError`],
/// because there is no value to hand back.
#[doc(hidden)]
pub async fn decode_lenient<T: for<'de> Lenient<'de>>(
    response: ::reqwest::Response,
    root: &'static Site,
) -> Result<ResponseValue<T>, DecodeError> {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?;
    // The same rule as the strict decoder: an empty body is `null`, which is what an optional
    // root accepts and what a 204 actually sends.
    let slice: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
    let decoded =
        decode_lenient_slice(slice, root).map_err(|error| DecodeError::new(status, error))?;
    Ok(ResponseValue::decoded(status, headers, decoded))
}

/// The lenient decode of one JSON document, held to the whole of it.
///
/// Leniency is about the shape of what was sent, not about what was sent: a body that goes on
/// after its value is not a JSON document, and `serde_json::from_slice` — which the strict
/// decoder reads with — refuses it too.
fn decode_lenient_slice<T: for<'de> Lenient<'de>>(
    slice: &[u8],
    root: &'static Site,
) -> Result<Decoded<T>, ::serde_json::Error> {
    let mut deserializer = ::serde_json::Deserializer::from_slice(slice);
    let decoded = T::decode(&mut deserializer, root)?;
    deserializer.end()?;
    Ok(decoded)
}

/// Read a text body directly from the response bytes.
#[doc(hidden)]
pub async fn decode_text(
    response: ::reqwest::Response,
) -> Result<ResponseValue<::std::string::String>, DecodeError> {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?;
    let value = ::std::string::String::from_utf8(bytes.to_vec())
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?;
    Ok(ResponseValue::new(status, headers, value))
}

/// Read a binary body directly from the response bytes.
#[doc(hidden)]
pub async fn decode_bytes(
    response: ::reqwest::Response,
) -> Result<ResponseValue<::std::vec::Vec<u8>>, DecodeError> {
    let status = response.status();
    let headers = response.headers().clone();
    let value = response
        .bytes()
        .await
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?
        .to_vec();
    Ok(ResponseValue::new(status, headers, value))
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::super::Site;

    const ROOT: &Site = &Site {
        type_name: "list",
        origin: "/paths/~1list/get/responses/200",
        member: None,
    };

    /// Leniency is about the shape of what was sent, not about what was sent: a body that goes
    /// on after its value is not a JSON document and is refused, the way the strict decoder
    /// refuses it.
    #[test_util::test]
    fn a_body_that_goes_on_after_its_value_is_refused() {
        let decoded = super::decode_lenient_slice::<Vec<i64>>(b"[1, 2] ", ROOT)?;
        assert_eq!(decoded.value, [1, 2]);
        let refused = super::decode_lenient_slice::<Vec<i64>>(b"[1, 2] garbage", ROOT);
        let message = refused.err().map(|err| err.to_string()).unwrap_or_default();
        assert!(message.contains("trailing"), "{message}");
    }
}
