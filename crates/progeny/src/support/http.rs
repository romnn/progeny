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
///
/// The body's bytes are kept beside the value only when the request asked for them with
/// `keep_raw_body`; see [`ResponseValue::raw_body`].
#[derive(Debug, Clone)]
pub struct ResponseValue<T> {
    status: ::reqwest::StatusCode,
    headers: ::reqwest::header::HeaderMap,
    value: T,
    degradations: Degradations,
    raw_body: Option<Vec<u8>>,
}

impl<T> ResponseValue<T> {
    pub(crate) fn new(
        status: ::reqwest::StatusCode,
        headers: ::reqwest::header::HeaderMap,
        value: T,
        raw_body: Option<Vec<u8>>,
    ) -> Self {
        Self {
            status,
            headers,
            value,
            degradations: Degradations::new(),
            raw_body,
        }
    }

    pub(crate) fn decoded(
        status: ::reqwest::StatusCode,
        headers: ::reqwest::header::HeaderMap,
        decoded: Decoded<T>,
        raw_body: Option<Vec<u8>>,
    ) -> Self {
        Self {
            status,
            headers,
            value: decoded.value,
            degradations: decoded.degradations,
            raw_body,
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

    /// The body exactly as it arrived, when the request asked to keep it.
    ///
    /// `Some` only for a response to a request sent with `keep_raw_body`, and then always: the
    /// bytes are the whole body as the transport decoded it, before any parsing — members the
    /// description never declared, `null`s, number spellings and element order included.
    /// An empty body is an empty slice, not the `null` a decoder reads it as.
    ///
    /// For a caller that has to store or forward what the server sent rather than what the
    /// description says it sends.
    /// The bytes are held as well as the value, so a kept response costs the body's length again
    /// in memory.
    pub fn raw_body(&self) -> Option<&[u8]> {
        self.raw_body.as_deref()
    }

    /// Take the body exactly as it arrived, discarding everything else.
    ///
    /// `None` unless the request asked to keep it, as for [`ResponseValue::raw_body`].
    pub fn into_raw_body(self) -> Option<Vec<u8>> {
        self.raw_body
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
            raw_body: self.raw_body,
        }
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
    /// A response body longer than the limit the client or the request set with
    /// `response_body_limit`.
    ///
    /// Refused before the body is held whole: a declared `Content-Length` past the limit is
    /// refused unread, and a body of unknown length is read only until it passes the limit.
    /// It applies to every body the client reads, a declared error's included.
    /// An [`Error::UnexpectedStatus`] response is handed back unread, so its body is the
    /// caller's to bound.
    #[error(transparent)]
    BodyTooLarge(#[from] BodyTooLarge),
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
    /// The page itself is carried whole — its status, its headers and its report — with the body
    /// set aside, because a stream cannot hand a body back through an error and everything else
    /// about the response is what a caller needs to decide what happened.
    /// Its raw bytes stay with it when the request kept them.
    ///
    /// Only a leniently decoded client can produce this: a strict decode fails the page as a
    /// [`Error::Decode`] before there is a report to look at.
    #[error(
        "a page of the stream was degraded on the path to its items or next cursor: {}",
        .0.degradations()
    )]
    DegradedPage(::std::boxed::Box<ResponseValue<()>>),
    /// A path parameter whose rendered segment is empty, `.` or `..`.
    ///
    /// Every WHATWG URL parser — reqwest's included — folds dot segments away before the
    /// request leaves, so `/files/{id}/metadata` with `id = ".."` would silently ask for
    /// `/metadata`: another endpoint entirely. Percent-encoding cannot save the spelling
    /// (`%2E` segments normalize the same way), so the request is refused before it is built.
    ///
    /// An empty segment is refused for the same reason: `/files/{id}` with `id = ""` asks for
    /// `/files/`, which a server routes as the collection rather than as one of its members.
    #[error(
        "path parameter `{parameter}` renders the segment `{rendered}`, which would address \
         another endpoint's path"
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
            Self::Decode(error) => Some(error.status()),
            Self::BodyTooLarge(error) => Some(error.status()),
            Self::DegradedPage(page) => Some(page.status()),
            Self::UnsendablePath { .. } => None,
        }
    }
}

impl<E> From<BodyError> for Error<E> {
    fn from(error: BodyError) -> Self {
        match error {
            BodyError::Decode(error) => Self::Decode(error),
            BodyError::TooLarge(error) => Self::BodyTooLarge(error),
        }
    }
}

/// A response body longer than the limit its request allowed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the {status} response body exceeds the limit of {limit} bytes")]
pub struct BodyTooLarge {
    status: ::reqwest::StatusCode,
    limit: usize,
}

impl BodyTooLarge {
    /// The status whose body was refused.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }

    /// The limit the body passed, in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// Why a body could not be read into its value: the narrow error the decoders return, which
/// `?` widens into [`Error`].
///
/// Not generic over the operation's error payload, for the reason [`to_value`] gives.
#[doc(hidden)]
#[derive(Debug)]
pub enum BodyError {
    Decode(DecodeError),
    TooLarge(BodyTooLarge),
}

impl From<DecodeError> for BodyError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<BodyTooLarge> for BodyError {
    fn from(error: BodyTooLarge) -> Self {
        Self::TooLarge(error)
    }
}

/// How one request reads its response body: up to which size, and whether its bytes are kept.
///
/// Carried by every generated request and set through its `response_body_limit` and
/// `keep_raw_body`.
/// The default reads the whole body, however long, and keeps only the value parsed from it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BodyReading {
    limit: Limit,
    keep: bool,
}

/// A reading's limit, which has three states where its setter's `Option<usize>` has two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Limit {
    /// Never set: a request inherits the client's limit, and a client reads every body whole.
    #[default]
    Inherit,
    /// Refuse a body longer than this many bytes.
    Bytes(usize),
    /// Read every body whole, whatever the client's limit.
    Unlimited,
}

impl BodyReading {
    /// The same, refusing a body longer than `bytes`, or reading every body whole for `None`.
    pub fn limited(self, bytes: Option<usize>) -> Self {
        Self {
            limit: bytes.map_or(Limit::Unlimited, Limit::Bytes),
            ..self
        }
    }

    /// The same, keeping the body's bytes on the response.
    pub fn keeping(self) -> Self {
        Self { keep: true, ..self }
    }

    /// The same, with the client's limit where the request set none of its own.
    ///
    /// A request that set `None` has set a limit, of no bytes at all, so the client's does not
    /// apply.
    /// Only the limit has a client-wide setting; whether to keep the bytes is the request's.
    pub fn under(self, client: Self) -> Self {
        let limit = match self.limit {
            Limit::Inherit => client.limit,
            set @ (Limit::Bytes(_) | Limit::Unlimited) => set,
        };
        Self { limit, ..self }
    }
}

/// Read a whole body, refusing it once it passes the reading's limit.
///
/// A declared `Content-Length` past the limit is refused before a byte is read.
/// The length is only a claim — it is absent from a chunked body and from one the transport
/// decompresses — so the limit is also held while reading, which bounds what is held in memory
/// whatever the server said.
async fn read_body(
    mut response: ::reqwest::Response,
    reading: BodyReading,
) -> Result<Vec<u8>, BodyError> {
    let status = response.status();
    let unreadable =
        |error: ::reqwest::Error| DecodeError::new(status, ::serde::de::Error::custom(error));
    let Limit::Bytes(limit) = reading.limit else {
        let bytes = response.bytes().await.map_err(unreadable)?;
        return Ok(Vec::from(bytes));
    };
    let too_large = BodyTooLarge { status, limit };
    let declared = response.content_length();
    if declared.is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX)) {
        return Err(too_large.into());
    }
    let capacity = declared
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(limit);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(unreadable)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(too_large.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A body that did not match the type the description said it would be.
#[derive(Debug, thiserror::Error)]
#[error("the {status} response did not match the shape the description declares: {source}")]
pub struct DecodeError {
    status: ::reqwest::StatusCode,
    source: ::serde_json::Error,
}

impl DecodeError {
    pub(crate) fn new(status: ::reqwest::StatusCode, source: ::serde_json::Error) -> Self {
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

/// Add a header that carries a credential, with its value marked sensitive.
///
/// A sensitive value prints as `Sensitive` in the `Debug` output of the request, and of anything
/// that holds it, so a logged request or error does not spell the secret.
/// HTTP/2 also tells intermediaries never to index it.
/// A value that is not a valid header value goes through reqwest's own conversion instead, so it
/// fails the request at `send` exactly as any other header value would.
#[doc(hidden)]
pub fn sensitive_header(
    request: ::reqwest::RequestBuilder,
    name: &'static str,
    value: ::std::string::String,
) -> ::reqwest::RequestBuilder {
    match ::reqwest::header::HeaderValue::from_str(&value) {
        Ok(mut header) => {
            header.set_sensitive(true);
            request.header(name, header)
        }
        Err(_) => request.header(name, value),
    }
}

/// Parse a successful body, or say the contract was wrong about it.
///
/// One non-generic-per-operation helper rather than an inline block per `send()`: the body of this
/// is compiled once per response type instead of once per operation.
/// The same holds for the reading settings, which arrive as a value rather than as code in each
/// `send()`.
#[doc(hidden)]
pub async fn decode_json<T: ::serde::de::DeserializeOwned>(
    response: ::reqwest::Response,
    reading: BodyReading,
) -> Result<ResponseValue<T>, BodyError> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_body(response, reading).await?;
    // An empty body deserializes as `null`, which is what `()` and `Option<T>` accept and what a
    // 204 actually sends. Without this a declared-but-empty success arm would fail to parse.
    let slice: &[u8] = if body.is_empty() { b"null" } else { &body };
    let value = ::serde_json::from_slice(slice).map_err(|error| DecodeError::new(status, error))?;
    Ok(ResponseValue::new(
        status,
        headers,
        value,
        reading.keep.then_some(body),
    ))
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
    reading: BodyReading,
    root: Site,
) -> Result<ResponseValue<T>, BodyError> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_body(response, reading).await?;
    // The same rule as the strict decoder: an empty body is `null`, which is what an optional
    // root accepts and what a 204 actually sends.
    let slice: &[u8] = if body.is_empty() { b"null" } else { &body };
    let decoded =
        decode_lenient_slice(slice, root).map_err(|error| DecodeError::new(status, error))?;
    Ok(ResponseValue::decoded(
        status,
        headers,
        decoded,
        reading.keep.then_some(body),
    ))
}

/// The lenient decode of one JSON document, held to the whole of it.
///
/// Leniency is about the shape of what was sent, not about what was sent: a body that goes on
/// after its value is not a JSON document, and `serde_json::from_slice` — which the strict
/// decoder reads with — refuses it too.
fn decode_lenient_slice<T: for<'de> Lenient<'de>>(
    slice: &[u8],
    root: Site,
) -> Result<Decoded<T>, ::serde_json::Error> {
    let mut deserializer = ::serde_json::Deserializer::from_slice(slice);
    let decoded = Decoded::from_deserializer_at(root, &mut deserializer)?;
    deserializer.end()?;
    Ok(decoded)
}

/// Read a text body directly from the response bytes.
#[doc(hidden)]
pub async fn decode_text(
    response: ::reqwest::Response,
    reading: BodyReading,
) -> Result<ResponseValue<::std::string::String>, BodyError> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_body(response, reading).await?;
    // The value takes the bytes, so a kept copy is made first.
    let raw_body = reading.keep.then(|| body.clone());
    let value = ::std::string::String::from_utf8(body)
        .map_err(|error| DecodeError::new(status, ::serde::de::Error::custom(error)))?;
    Ok(ResponseValue::new(status, headers, value, raw_body))
}

/// Read a binary body directly from the response bytes.
#[doc(hidden)]
pub async fn decode_bytes(
    response: ::reqwest::Response,
    reading: BodyReading,
) -> Result<ResponseValue<::std::vec::Vec<u8>>, BodyError> {
    let status = response.status();
    let headers = response.headers().clone();
    let value = read_body(response, reading).await?;
    let raw_body = reading.keep.then(|| value.clone());
    Ok(ResponseValue::new(status, headers, value, raw_body))
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::super::Site;

    const ROOT: Site = Site::new("list", "/paths/~1list/get/responses/200", None);

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

    /// A credential header stays out of the request's `Debug` output, and a value that is not a
    /// header value still fails the request the way an ordinary header does.
    #[test_util::test]
    fn a_credential_header_is_marked_sensitive() {
        let client = ::reqwest::Client::new();
        let request = super::sensitive_header(
            client.get("http://localhost/"),
            "Authorization",
            "Bearer secret".to_owned(),
        )
        .build()?;
        let header = request
            .headers()
            .get(::reqwest::header::AUTHORIZATION)
            .ok_or_else(|| eyre::eyre!("the credential header is set"))?;
        assert!(header.is_sensitive());
        assert_eq!(header.as_bytes(), b"Bearer secret");
        assert!(!format!("{request:?}").contains("secret"));

        let invalid = super::sensitive_header(
            client.get("http://localhost/"),
            "Authorization",
            "line\nbreak".to_owned(),
        )
        .build();
        assert!(invalid.is_err());
    }

    /// A request's own limit wins over the client's, the client's applies where the request set
    /// none, and a request's `None` lifts the client's limit rather than inheriting it.
    #[test_util::test]
    fn a_request_limit_overrides_the_client_default_and_can_lift_it() {
        use super::Limit::{Bytes, Inherit, Unlimited};

        let unset = super::BodyReading::default();
        let client = unset.limited(Some(10));
        // Not set: the request inherits the client's limit, or none when the client has none.
        assert_eq!(unset.under(client).limit, Bytes(10));
        assert_eq!(unset.under(unset).limit, Inherit);
        // A number of bytes replaces the client's.
        assert_eq!(unset.limited(Some(5)).under(client).limit, Bytes(5));
        // `None` is a setting of its own, so it lifts the client's limit rather than inheriting
        // it; on the client, it is the same as setting none.
        assert_eq!(unset.limited(None).under(client).limit, Unlimited);
        assert_eq!(unset.under(unset.limited(None)).limit, Unlimited);
        // Keeping the bytes is the request's alone.
        assert!(!unset.keep);
        assert!(unset.keeping().under(client).keep);
    }
}
