//! The client runtime: what every generated operation returns.
//!
//! Shipped verbatim into generated crates. It names `reqwest`, which progeny only carries as a
//! dev-dependency, so it compiles here under `cfg(test)` — same bytes tested as shipped — and again
//! on every generated crate the corpus compile gate checks, with `--all-features`.
//!
//! Error types use `thiserror` so their messages and source chains stay on the declarations they
//! describe rather than drifting into parallel implementations.

use super::{Decoded, Degradations};
// Only the lenient decoders name these, and strict decoding leaves the decoders — and this
// import beside them — out of the generated crate.
use super::{DegradationKind, Lenient, Site};

/// A successful response, with everything the caller might need about it.
///
/// Every header is on [`ResponseValue::headers`], raw and exactly as it arrived.
/// The ones the description declares for the response's arm are also read into typed fields,
/// beside the body in the arm's payload, so a body and its headers cannot come from two arms.
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

    /// Hand `read` the headers and the report, so a generated arm reads its declared headers
    /// into their typed fields and records what it tolerated beside what the body's decode did.
    ///
    /// The report is the response's own, so one observer call sees the body and the headers at
    /// once.
    #[doc(hidden)]
    pub fn read_headers<H>(
        &mut self,
        read: impl FnOnce(&::reqwest::header::HeaderMap, &mut Degradations) -> H,
    ) -> H {
        read(&self.headers, &mut self.degradations)
    }
}

/// Why a request did not produce a declared successful response.
///
/// `E` is the operation's declared error payload, so a caller matches on a type rather than
/// re-parsing a body the document already described.
///
/// The variants that carry a response's headers are boxed. This type is the `Err` half of every
/// operation's return, so its width is paid on the success path too — and unboxed, a `HeaderMap`
/// alone puts it past the width at which `clippy::result_large_err` calls a `Result` too expensive
/// to return by value, in the generated crate and in its consumer's build alike.
///
/// No error an operation returns names the URL it requested, in its `Display` or its `Debug`:
/// any part of a URL can be a credential — an access token in a path segment, an API key in the
/// query, a tenant in the host.
/// What the server sent is another matter: a declared payload, a degradation sample or a header
/// the caller reads off a response says whatever the server put in it.
/// So does an error a redirect policy raises, which `reqwest` can nest inside the one returned
/// with the redirect's target in it.
#[derive(thiserror::Error)]
pub enum Error<E> {
    /// The request never completed: DNS, TLS, connection, timeout.
    ///
    /// A failure after the status arrived, while reading the body, is an [`Error::BodyRead`]
    /// instead, which keeps the status.
    /// The error carries no URL.
    #[error("the request failed: {0}")]
    Request(#[source] ::reqwest::Error),
    /// A status the document declares as an error, with its payload parsed.
    #[error("the server answered {}", .0.status())]
    Declared(::std::boxed::Box<ResponseValue<E>>),
    /// A status the document does not declare at all, handed back raw.
    ///
    /// The response is whole, its URL and headers included, for a caller who asks it; this
    /// error's `Debug` shows only its status, because either can carry a credential.
    ///
    /// Undeclared rather than unexpected in the ordinary sense: a document that lists only `200`
    /// says nothing about `503`, and inventing a shape for it would be describing a payload
    /// progeny has never seen.
    #[error(
        "the server answered {}, which the description does not declare",
        .0.status()
    )]
    UnexpectedStatus(::std::boxed::Box<::reqwest::Response>),
    /// The status arrived and its body could not be read to the end: the connection reset or
    /// stalled past a timeout, the framing broke off, or the content encoding was corrupt.
    ///
    /// A transport failure rather than a payload one, so it is not an [`Error::Decode`]: the
    /// bytes that would have been decoded never all arrived.
    /// It applies to every body the client reads, a declared error's included, and carries
    /// that response's status.
    #[error(transparent)]
    BodyRead(#[from] BodyReadError),
    /// The body arrived and did not match the contract the document stated.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// A header the description declares for the response did not match its declaration: absent
    /// where it is required, repeated where it is one value, or not readable as its type.
    ///
    /// Its own variant rather than an [`Error::Decode`], because it says nothing about the body,
    /// which may have decoded perfectly well, or have been empty.
    /// Only a strictly decoded client produces it: a lenient one leaves the typed field `None`
    /// and reports the header in the response's degradations instead.
    /// Boxed, like the other variants that carry a response's headers.
    #[error(transparent)]
    HeaderDecode(::std::boxed::Box<HeaderDecodeError>),
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

impl<E> From<::reqwest::Error> for Error<E> {
    fn from(error: ::reqwest::Error) -> Self {
        Self::Request(error.without_url())
    }
}

impl<E: ::std::fmt::Debug> ::std::fmt::Debug for Error<E> {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match self {
            Self::Request(error) => formatter.debug_tuple("Request").field(error).finish(),
            Self::Declared(response) => formatter.debug_tuple("Declared").field(response).finish(),
            // A response's own `Debug` prints the URL it answered and its headers.
            Self::UnexpectedStatus(response) => formatter
                .debug_struct("UnexpectedStatus")
                .field("status", &response.status())
                .finish_non_exhaustive(),
            Self::BodyRead(error) => formatter.debug_tuple("BodyRead").field(error).finish(),
            Self::Decode(error) => formatter.debug_tuple("Decode").field(error).finish(),
            Self::HeaderDecode(error) => {
                formatter.debug_tuple("HeaderDecode").field(error).finish()
            }
            Self::BodyTooLarge(error) => {
                formatter.debug_tuple("BodyTooLarge").field(error).finish()
            }
            Self::DegradedPage(page) => formatter.debug_tuple("DegradedPage").field(page).finish(),
            Self::UnsendablePath {
                parameter,
                rendered,
            } => formatter
                .debug_struct("UnsendablePath")
                .field("parameter", parameter)
                .field("rendered", rendered)
                .finish(),
        }
    }
}

impl<E> Error<E> {
    /// The status, where there was one.
    pub fn status(&self) -> Option<::reqwest::StatusCode> {
        match self {
            Self::Request(error) => error.status(),
            Self::Declared(response) => Some(response.status()),
            Self::UnexpectedStatus(response) => Some(response.status()),
            Self::BodyRead(error) => Some(error.status()),
            Self::Decode(error) => Some(error.status()),
            Self::HeaderDecode(error) => Some(error.status()),
            Self::BodyTooLarge(error) => Some(error.status()),
            Self::DegradedPage(page) => Some(page.status()),
            Self::UnsendablePath { .. } => None,
        }
    }
}

impl<E> From<::std::boxed::Box<HeaderDecodeError>> for Error<E> {
    fn from(error: ::std::boxed::Box<HeaderDecodeError>) -> Self {
        Self::HeaderDecode(error)
    }
}

impl<E> From<BodyError> for Error<E> {
    fn from(error: BodyError) -> Self {
        match error {
            BodyError::Read(error) => Self::BodyRead(error),
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
    Read(BodyReadError),
    Decode(DecodeError),
    TooLarge(BodyTooLarge),
}

impl From<BodyReadError> for BodyError {
    fn from(error: BodyReadError) -> Self {
        Self::Read(error)
    }
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
    // The URL is dropped from the error itself rather than from a message about it, because the
    // error is handed back whole: any part of it can be a credential, and the status already
    // places the failure.
    let unreadable = |error: ::reqwest::Error| BodyReadError {
        status,
        source: error.without_url(),
    };
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

/// A response body that could not be read to the end, after its status arrived.
///
/// Every failure of the read itself lands here, whatever `reqwest` calls it: it reports a body
/// that broke off as a decoding failure too, so its own classification cannot tell the two
/// apart.
/// What was read is not decoded.
#[derive(Debug, thiserror::Error)]
#[error("could not read the {status} response body: {source}")]
pub struct BodyReadError {
    status: ::reqwest::StatusCode,
    source: ::reqwest::Error,
}

impl BodyReadError {
    /// The status whose body could not be read.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }

    /// The failure itself, for `reqwest`'s own questions about it, such as
    /// [`is_timeout`](::reqwest::Error::is_timeout).
    ///
    /// It carries no URL, any part of which can be a credential.
    pub fn reqwest_error(&self) -> &::reqwest::Error {
        &self.source
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
    pub(crate) fn new(status: ::reqwest::StatusCode, source: ::serde_json::Error) -> Self {
        Self { status, source }
    }

    /// The status whose body failed to parse.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }
}

/// A header the description declares for a response, which did not match its declaration under
/// strict decoding.
///
/// Carries every header the response arrived with, exactly as it arrived, for a caller who wants
/// what the server sent; its `Debug` leaves them out, because a header can carry a credential.
#[derive(thiserror::Error)]
#[error("the `{name}` header of the {status} response does not match its declaration")]
pub struct HeaderDecodeError {
    status: ::reqwest::StatusCode,
    name: &'static str,
    origin: &'static str,
    #[source]
    problem: HeaderProblem,
    headers: ::reqwest::header::HeaderMap,
}

impl HeaderDecodeError {
    /// The status of the response the header arrived with.
    pub fn status(&self) -> ::reqwest::StatusCode {
        self.status
    }

    /// The header, by the name the description spells it with.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Where the description declares the header, as a JSON Pointer: the pointer a lenient
    /// client's degradation of the same header names.
    pub fn origin(&self) -> &'static str {
        self.origin
    }

    /// What was wrong with the header.
    pub fn problem(&self) -> &HeaderProblem {
        &self.problem
    }

    /// Every header the response arrived with, exactly as it arrived.
    pub fn headers(&self) -> &::reqwest::header::HeaderMap {
        &self.headers
    }
}

impl ::std::fmt::Debug for HeaderDecodeError {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter
            .debug_struct("HeaderDecodeError")
            .field("status", &self.status)
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("problem", &self.problem)
            .finish_non_exhaustive()
    }
}

/// What was wrong with a declared response header.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HeaderProblem {
    /// The description requires the header, and the response did not carry it.
    #[error("the header is required and was absent")]
    Absent,
    /// The header arrived in more than one field where the description declares one value.
    ///
    /// Joined, the fields would be a list, which is a different value from each of them, and
    /// reading only one of them would be an arbitrary pick.
    #[error("the header arrived in {count} fields, where the description declares one value")]
    Repeated {
        /// How many fields carried the header.
        count: usize,
    },
    /// A field holds bytes that are not UTF-8 text, which no declared type is read from.
    #[error("the header is not UTF-8 text")]
    NotText,
    /// The header's text does not read as its declared type.
    #[error("the header does not read as its declared type")]
    Malformed(#[source] ::serde_json::Error),
}

/// One declared response header, as a generated arm hands it to the header decoders.
///
/// The rules for reading a header live here, once, and a generated arm only says which apply.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct HeaderRow {
    /// The field name the description spells.
    pub name: &'static str,
    /// Where the description declares the header, as a JSON Pointer.
    pub origin: &'static str,
    /// Whether the description requires the header.
    pub required: bool,
    /// Whether the value is one JSON document, which is read whole.
    pub json: bool,
    /// Whether the value is a comma-separated list, gathered across every field it arrived in.
    pub list: bool,
    /// Whether a scalar's text, or each element's, is read as the boolean or number it spells
    /// rather than kept as text.
    pub spelled: bool,
}

/// One declared header as the JSON value its declaration reads it as, or `None` when the
/// response does not carry it.
///
/// Every field of the name is read, never only the first.
/// A list gathers the elements of all of them, because HTTP lets a sender split one list across
/// several fields (RFC 9110, section 5.3), and drops the empty elements its list syntax allows.
/// A header declared as one value that arrived in more than one field is refused rather than
/// read as whichever came first.
/// A scalar is never split on its commas, which a string such as a `Link` holds.
fn header_value(
    headers: &::reqwest::header::HeaderMap,
    row: &HeaderRow,
) -> Result<Option<::serde_json::Value>, HeaderProblem> {
    // UTF-8 rather than the visible ASCII `to_str` insists on: a server that writes a filename or
    // a message in UTF-8 has sent text, and refusing it would lose a value nobody misread.
    let fields = headers
        .get_all(row.name)
        .iter()
        .map(|field| std::str::from_utf8(field.as_bytes()).map_err(|_| HeaderProblem::NotText))
        .collect::<Result<Vec<&str>, HeaderProblem>>()?;
    if fields.is_empty() {
        return Ok(None);
    }
    if row.list && !row.json {
        let elements = fields
            .iter()
            .flat_map(|field| field.split(','))
            .map(|element| element.trim_matches([' ', '\t']))
            .filter(|element| !element.is_empty())
            .map(|element| header_scalar(element, row.spelled))
            .collect();
        return Ok(Some(::serde_json::Value::Array(elements)));
    }
    let [text] = fields.as_slice() else {
        return Err(HeaderProblem::Repeated {
            count: fields.len(),
        });
    };
    if row.json {
        return ::serde_json::from_str(text)
            .map(Some)
            .map_err(HeaderProblem::Malformed);
    }
    Ok(Some(header_scalar(text, row.spelled)))
}

/// One scalar's text as the value it reads back as: the boolean or number it spells, where the
/// declaration reads its scalars that way and the text spells one, and the text itself otherwise.
///
/// Anything else stays text, so the type decides: `soon` is a malformed integer.
/// A declaration that reads its scalars as text keeps `true` and `7` as the strings they are.
fn header_scalar(text: &str, spelled: bool) -> ::serde_json::Value {
    if spelled
        && let Ok(parsed @ (::serde_json::Value::Bool(_) | ::serde_json::Value::Number(_))) =
            ::serde_json::from_str(text)
    {
        return parsed;
    }
    ::serde_json::Value::String(text.to_owned())
}

/// One declared header read leniently into its typed field: `None`, with the reason recorded,
/// when it is malformed, and `None`, with the absence recorded, when the description requires it
/// and the response does not carry it.
///
/// Recorded at the header's own site: the generated `group` its field belongs to, and the place
/// the description declares the header.
/// An absent optional header is no drift, and neither is a header nobody declared, which never
/// reaches this.
/// Compiled once per header type, like the body decoders.
#[doc(hidden)]
pub fn lenient_header<T: for<'de> Lenient<'de>>(
    headers: &::reqwest::header::HeaderMap,
    report: &mut Degradations,
    group: &'static str,
    row: &HeaderRow,
) -> Option<T> {
    let site = Site::new(group, row.origin, None);
    let refused = match header_value(headers, row) {
        Ok(None) => {
            if row.required {
                report.record(site, DegradationKind::RequiredAbsent, None, true);
            }
            return None;
        }
        Ok(Some(value)) => match Decoded::<T>::from_deserializer_at(site, value) {
            Ok(decoded) => {
                report.merge(decoded.degradations);
                return Some(decoded.value);
            }
            Err(err) => err.to_string(),
        },
        // The parser's own words say what was wrong with the document.
        Err(HeaderProblem::Malformed(err)) => err.to_string(),
        Err(problem) => problem.to_string(),
    };
    report.record(site, DegradationKind::Undecodable, Some(refused), true);
    None
}

/// One declared header read strictly into its typed field, or the reason the response fails.
///
/// `T` is the field's type, which is an `Option` exactly when the description lets the header
/// be absent; an absent optional header reads as `None`.
/// Compiled once per header type, like the body decoders.
///
/// # Errors
///
/// Returns a [`HeaderDecodeError`] when a required header is absent, when one declared as one
/// value arrived in more than one field, and when one does not read as its declared type.
#[doc(hidden)]
pub fn strict_header<T: ::serde::de::DeserializeOwned>(
    status: ::reqwest::StatusCode,
    headers: &::reqwest::header::HeaderMap,
    row: &HeaderRow,
) -> Result<T, ::std::boxed::Box<HeaderDecodeError>> {
    let refused = |problem| {
        ::std::boxed::Box::new(HeaderDecodeError {
            status,
            name: row.name,
            origin: row.origin,
            problem,
            headers: headers.clone(),
        })
    };
    let value = match header_value(headers, row) {
        Ok(Some(value)) => value,
        Ok(None) if row.required => return Err(refused(HeaderProblem::Absent)),
        // An absent optional header is `None`, which every optional field is typed to hold.
        Ok(None) => ::serde_json::Value::Null,
        Err(problem) => return Err(refused(problem)),
    };
    ::serde_json::from_value(value).map_err(|err| refused(HeaderProblem::Malformed(err)))
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
    use color_eyre::eyre::{self, OptionExt as _};

    use super::super::Site;

    const ROOT: Site = Site::new("list", "/paths/~1list/get/responses/200", None);

    /// A `reqwest` error converted into the client's carries no part of the URL it was about:
    /// every part of one can be a credential, the host included.
    #[test_util::test]
    fn a_transport_error_loses_its_whole_url_on_the_way_in() {
        // Any `reqwest` error will do; this one is refused before it is sent.
        let refused = ::reqwest::Client::new()
            .get("http://[")
            .build()
            .err()
            .ok_or_eyre("an unparsable URL is refused")?;
        let url = ::reqwest::Url::parse(
            "https://user:hunter2@tenant.example/tokens/t0k3n?api_key=k3y#fragment",
        )?;
        let error: super::Error<()> = refused.with_url(url).into();
        let super::Error::Request(inner) = &error else {
            eyre::bail!("expected Request, got {error:?}");
        };
        assert!(inner.url().is_none(), "{:?}", inner.url());
        for rendered in [error.to_string(), format!("{error:?}")] {
            for secret in ["user", "hunter2", "tenant", "t0k3n", "k3y", "fragment"] {
                assert!(!rendered.contains(secret), "`{secret}` in {rendered}");
            }
        }
    }

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

    /// The fields a response arrived with, in order, repeated names included.
    fn arrived(fields: &[(&'static str, &[u8])]) -> eyre::Result<::reqwest::header::HeaderMap> {
        let mut headers = ::reqwest::header::HeaderMap::new();
        for (name, value) in fields {
            headers.append(*name, ::reqwest::header::HeaderValue::from_bytes(value)?);
        }
        Ok(headers)
    }

    /// The row of an optional header declared as one value its text is.
    const TEXT: super::HeaderRow = super::HeaderRow {
        name: "X-Note",
        origin: "/paths/~1x/get/responses/200/headers/X-Note",
        required: false,
        json: false,
        list: false,
        spelled: false,
    };

    /// The group a lenient read records at, as a generated reader names it.
    const GROUP: &str = "GetXOkHeaders";

    /// One header read leniently, with what reading it recorded.
    fn lenient<T: for<'de> super::Lenient<'de>>(
        headers: &::reqwest::header::HeaderMap,
        row: &super::HeaderRow,
    ) -> (Option<T>, super::Degradations) {
        let mut report = super::Degradations::rooted(ROOT);
        let value = super::lenient_header(headers, &mut report, GROUP, row);
        (value, report)
    }

    /// One header read strictly into a field of type `T`.
    fn strict<T: ::serde::de::DeserializeOwned>(
        headers: &::reqwest::header::HeaderMap,
        row: &super::HeaderRow,
    ) -> Result<T, Box<super::HeaderDecodeError>> {
        super::strict_header(::reqwest::StatusCode::ACCEPTED, headers, row)
    }

    /// The only entry of a report, which is keyed at the header's own site.
    fn only_entry(
        report: &super::Degradations,
    ) -> eyre::Result<super::super::degradations::Degradation<'_>> {
        let mut entries = report.iter();
        let entry = entries.next().ok_or_eyre("one entry")?;
        eyre::ensure!(entries.next().is_none(), "one entry only: {report}");
        assert_eq!(entry.site.type_name(), GROUP);
        assert_eq!(entry.site.member(), None);
        Ok(entry)
    }

    /// A string enum read the way a generated one is: an unlisted value kept, and reported.
    #[derive(Debug, PartialEq, ::serde::Deserialize)]
    enum Mode {
        #[serde(rename = "fast")]
        Fast,
        #[serde(untagged)]
        Unknown(String),
    }

    impl<'de> super::Lenient<'de> for Mode {
        fn lenient(
            content: super::super::Content<'de>,
            site: Site,
            report: &mut super::Degradations,
        ) -> Result<Self, super::super::lenient::Problem> {
            super::super::lenient::open_string(content, site, report, |mode| match mode {
                Mode::Fast => None,
                Mode::Unknown(raw) => Some(raw.as_str()),
            })
        }
    }

    /// A header declared as one value is its whole text, commas and all, which a `Link` and a
    /// date both carry, under either decoding.
    #[test_util::test]
    fn a_header_declared_as_one_value_is_never_split_on_its_commas() {
        let link = r#"<https://x.test/?page=2>; rel="next", <https://x.test/?page=9>; rel="last""#;
        let headers = arrived(&[("x-note", link.as_bytes())])?;
        assert_eq!(
            strict::<Option<String>>(&headers, &TEXT)?.as_deref(),
            Some(link)
        );
        let (value, report) = lenient::<String>(&headers, &TEXT);
        assert_eq!(value.as_deref(), Some(link));
        assert!(report.is_empty(), "{report}");
    }

    /// A list gathers its elements across every field it arrived in, as HTTP lets a sender split
    /// one, with the whitespace around each element and the empty elements its syntax allows
    /// dropped, and each element read the way the declaration reads its scalars.
    #[test_util::test]
    fn a_list_gathers_its_elements_across_every_field() {
        let list = super::HeaderRow { list: true, ..TEXT };
        let headers = arrived(&[("x-note", b"a, b,,"), ("x-note", b"\tc ")])?;
        assert_eq!(
            strict::<Option<Vec<String>>>(&headers, &list)?,
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        let numbers = super::HeaderRow {
            spelled: true,
            ..list
        };
        let headers = arrived(&[("x-note", b"1,2"), ("x-note", b"3")])?;
        assert_eq!(strict::<Vec<i64>>(&headers, &numbers)?, [1, 2, 3]);

        // Leniently, an element that does not read is left out and counted, and the rest stay.
        let headers = arrived(&[("x-note", b"1,two,3")])?;
        let (value, report) = lenient::<Vec<i64>>(&headers, &numbers);
        assert_eq!(value, Some(vec![1, 3]));
        let entry = only_entry(&report)?;
        assert_eq!(entry.kind, super::DegradationKind::SkippedElement);
        assert_eq!(entry.site.origin(), TEXT.origin);
    }

    /// A header declared as one value that arrived in two fields is refused, never read as the
    /// first or the last: a strict client fails the response, and a lenient one leaves the field
    /// `None` and says why.
    #[test_util::test]
    fn a_singleton_that_arrives_repeated_is_refused() {
        let headers = arrived(&[("x-note", b"first"), ("x-note", b"second")])?;
        let refused = strict::<Option<String>>(&headers, &TEXT)
            .err()
            .ok_or_eyre("a repeated singleton is refused")?;
        assert!(
            matches!(
                refused.problem(),
                super::HeaderProblem::Repeated { count: 2 }
            ),
            "{refused:?}"
        );
        let (value, report) = lenient::<String>(&headers, &TEXT);
        assert_eq!(value, None);
        let entry = only_entry(&report)?;
        assert_eq!(entry.kind, super::DegradationKind::Undecodable);
        assert!(
            entry
                .samples
                .iter()
                .any(|sample| sample.contains("2 fields")),
            "{report}"
        );
    }

    /// A header declared with a JSON `content` is one document, read whole: its commas are the
    /// document's own, and text that is not JSON does not read.
    #[test_util::test]
    fn a_json_content_header_is_one_document() {
        let json = super::HeaderRow { json: true, ..TEXT };
        let headers = arrived(&[("x-note", br#"{"side": 3, "tags": ["a,b", "c"]}"#)])?;
        assert_eq!(
            strict::<Option<::serde_json::Value>>(&headers, &json)?,
            Some(::serde_json::json!({"side": 3, "tags": ["a,b", "c"]}))
        );
        let headers = arrived(&[("x-note", b"{not json")])?;
        let refused = strict::<Option<::serde_json::Value>>(&headers, &json)
            .err()
            .ok_or_eyre("text that is not JSON is refused")?;
        assert!(
            matches!(refused.problem(), super::HeaderProblem::Malformed(_)),
            "{refused:?}"
        );
        let (value, report) = lenient::<::serde_json::Value>(&headers, &json);
        assert_eq!(value, None);
        assert_eq!(
            only_entry(&report)?.kind,
            super::DegradationKind::Undecodable
        );
    }

    /// A number or a boolean is the JSON its text spells, and text that spells neither is
    /// malformed for its type; a header read as its text keeps text that spells one as the
    /// string it is.
    #[test_util::test]
    fn a_number_or_a_boolean_reads_from_the_text_it_spells() {
        let spelled = super::HeaderRow {
            spelled: true,
            ..TEXT
        };
        let header = |text: &'static str| arrived(&[("x-note", text.as_bytes())]);
        assert_eq!(strict::<Option<i64>>(&header("30")?, &spelled)?, Some(30));
        assert_eq!(strict::<Option<f64>>(&header("0.5")?, &spelled)?, Some(0.5));
        assert_eq!(
            strict::<Option<bool>>(&header("true")?, &spelled)?,
            Some(true)
        );
        // A value its schema says nothing about is typed as its text, so `true` and `7` stay the
        // strings a server wrote.
        for text in ["true", "7"] {
            assert_eq!(
                strict::<Option<String>>(&header(text)?, &TEXT)?.as_deref(),
                Some(text)
            );
        }

        // `Retry-After: soon` where the description declares seconds
        let soon = header("soon")?;
        let refused = strict::<Option<i64>>(&soon, &spelled)
            .err()
            .ok_or_eyre("a malformed integer is refused")?;
        assert_eq!(refused.status(), ::reqwest::StatusCode::ACCEPTED);
        assert_eq!(refused.name(), "X-Note");
        assert_eq!(refused.origin(), TEXT.origin);
        assert!(matches!(
            refused.problem(),
            super::HeaderProblem::Malformed(_)
        ));
        assert_eq!(refused.headers(), &soon);
        let (value, report) = lenient::<i64>(&soon, &spelled);
        assert_eq!(value, None);
        let entry = only_entry(&report)?;
        assert_eq!(entry.kind, super::DegradationKind::Undecodable);
        assert_eq!(entry.site.pointer(), TEXT.origin);
        assert!(
            entry.samples.iter().any(|sample| sample.contains("soon")),
            "{report}"
        );
    }

    /// A string enum reads its text, and an unlisted value lands in its open arm: a strict
    /// client accepts it, and a lenient one keeps it and reports it at the header's site.
    #[test_util::test]
    fn a_string_enum_reads_its_text_and_keeps_an_unlisted_value() {
        let fast = arrived(&[("x-note", b"fast")])?;
        assert_eq!(strict::<Option<Mode>>(&fast, &TEXT)?, Some(Mode::Fast));
        let medium = arrived(&[("x-note", b"medium")])?;
        assert_eq!(
            strict::<Option<Mode>>(&medium, &TEXT)?,
            Some(Mode::Unknown("medium".to_owned()))
        );
        let (value, report) = lenient::<Mode>(&medium, &TEXT);
        assert_eq!(value, Some(Mode::Unknown("medium".to_owned())));
        let entry = only_entry(&report)?;
        assert_eq!(entry.kind, super::DegradationKind::UnknownEnumValue);
        assert_eq!(entry.samples, ["medium"]);
    }

    /// A required header that is absent fails a strict response and is reported by a lenient
    /// one; an optional one that is absent is `None` and no drift; a field that is not UTF-8
    /// reads as nothing, and one that is reads as its text.
    #[test_util::test]
    fn absence_is_drift_only_where_the_description_requires_the_header() {
        let required = super::HeaderRow {
            required: true,
            ..TEXT
        };
        let none = arrived(&[("x-other", b"undeclared")])?;
        let refused = strict::<String>(&none, &required)
            .err()
            .ok_or_eyre("an absent required header is refused")?;
        assert!(matches!(refused.problem(), super::HeaderProblem::Absent));
        let (value, report) = lenient::<String>(&none, &required);
        assert_eq!(value, None);
        assert_eq!(
            only_entry(&report)?.kind,
            super::DegradationKind::RequiredAbsent
        );

        assert_eq!(strict::<Option<String>>(&none, &TEXT)?, None);
        let (value, report) = lenient::<String>(&none, &TEXT);
        assert_eq!(value, None);
        assert!(report.is_empty(), "{report}");

        let latin = arrived(&[("x-note", b"na\xefve")])?;
        let refused = strict::<Option<String>>(&latin, &TEXT)
            .err()
            .ok_or_eyre("a field that is not UTF-8 is refused")?;
        assert!(matches!(refused.problem(), super::HeaderProblem::NotText));
        let utf8 = arrived(&[("x-note", "naïve".as_bytes())])?;
        assert_eq!(
            strict::<Option<String>>(&utf8, &TEXT)?.as_deref(),
            Some("naïve")
        );
    }

    /// A header read leniently joins the response's own report, beside what the body's decode
    /// recorded, so one look at the response sees both.
    #[test_util::test]
    fn a_lenient_header_read_joins_the_response_report() {
        let headers = arrived(&[("x-note", b"soon")])?;
        let mut body = super::Degradations::rooted(ROOT);
        body.record(
            Site::new("Pet", "/components/schemas/Pet", Some("name")),
            super::DegradationKind::RequiredAbsent,
            None,
            false,
        );
        let mut response = super::ResponseValue::decoded(
            ::reqwest::StatusCode::OK,
            headers,
            super::Decoded {
                value: (),
                degradations: body,
            },
            None,
        );
        let spelled = super::HeaderRow {
            spelled: true,
            ..TEXT
        };
        let read: Option<i64> = response.read_headers(|headers, report| {
            super::lenient_header(headers, report, GROUP, &spelled)
        });
        assert_eq!(read, None);
        assert_eq!(
            response.degradations().len(),
            2,
            "{}",
            response.degradations()
        );
        assert_eq!(response.degradations().root(), ROOT);
    }

    /// A header error says which header and status, keeps the status the client's error reports,
    /// and leaves the headers out of its `Debug`, any of which can carry a credential.
    #[test_util::test]
    fn a_header_error_names_its_header_and_keeps_the_rest_out_of_debug() {
        let headers = arrived(&[
            ("x-note", b"first"),
            ("x-note", b"second"),
            ("x-token", b"s3cr3t"),
        ])?;
        let refused = strict::<Option<String>>(&headers, &TEXT)
            .err()
            .ok_or_eyre("a repeated singleton is refused")?;
        assert_eq!(
            refused.to_string(),
            "the `X-Note` header of the 202 Accepted response does not match its declaration"
        );
        let cause = ::std::error::Error::source(&*refused).map(ToString::to_string);
        assert_eq!(
            cause.as_deref(),
            Some("the header arrived in 2 fields, where the description declares one value")
        );
        let error: super::Error<()> = refused.into();
        assert_eq!(error.status(), Some(::reqwest::StatusCode::ACCEPTED));
        let debug = format!("{error:?}");
        assert!(debug.contains("HeaderDecode"), "{debug}");
        assert!(!debug.contains("s3cr3t"), "{debug}");
        let super::Error::HeaderDecode(refused) = error else {
            eyre::bail!("expected a header error, got {error:?}");
        };
        assert_eq!(refused.headers().get_all("x-note").iter().count(), 2);
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
