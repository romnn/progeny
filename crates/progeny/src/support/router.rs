//! What a generated handler does with an `axum` request, and what it sends back.
//!
//! The half of the serving runtime that names `axum`, kept apart from [`super::serve`] for the same
//! reason the client runtime is kept apart from the style table: `axum` is a dev-dependency only,
//! so this file compiles under `cfg(test)` here and in every generated crate the corpus compile
//! gate checks. The rules that can be unit-tested without a socket live next door in `serve`.

use std::collections::BTreeMap;

use serde_json::Value;

/// Why a generated server could not decode an incoming request.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// A header that must be text contained bytes outside UTF-8.
    #[error("a request header is not valid text")]
    Header(#[from] ::axum::http::header::ToStrError),
    /// Reading the request body from the connection failed.
    #[error("reading the request body failed")]
    Body(#[from] ::axum::Error),
    /// A JSON request body did not contain the declared type.
    #[error("the request body is not valid JSON")]
    Json(#[from] ::serde_json::Error),
    /// A multipart request did not declare its boundary.
    #[error("the request declares no multipart boundary")]
    MissingMultipartBoundary,
    /// A multipart request was malformed.
    #[error(transparent)]
    Multipart(#[from] super::multipart::ParseError),
    /// Text assembled from request fields did not match the declared type.
    #[error(transparent)]
    Decode(#[from] super::serve::ServerDecodeError),
    /// A text request body contained bytes outside UTF-8.
    #[error("the request body is not valid UTF-8")]
    Text(#[from] ::std::string::FromUtf8Error),
}

/// A request taken apart once, so each parameter is a lookup rather than another parse.
///
/// Built before any extraction runs. The alternative — one `FromRequestParts` per parameter — parses
/// the query string once per parameter, which for `jellyfin`'s fourteen-parameter operations is
/// fourteen passes over the same bytes.
pub struct Incoming {
    path: BTreeMap<String, String>,
    query: String,
    headers: ::axum::http::HeaderMap,
    body: ::axum::body::Body,
}

/// Take a request apart, from the head the context's extractor left behind.
///
/// Handed the head and the body rather than the request, because the handler already split it to
/// extract the context, and joining them again only to split them here would buy nothing.
pub async fn receive(
    mut parts: ::axum::http::request::Parts,
    body: ::axum::body::Body,
) -> Incoming {
    use ::axum::extract::FromRequestParts as _;

    // Asked for through the extractor rather than read out of the extensions: what the router
    // stores there is its own type, and reaching for anything else finds nothing — every path
    // parameter then arrives as "not sent", which is a request rejected for being exactly right.
    let path = ::axum::extract::RawPathParams::from_request_parts(&mut parts, &())
        .await
        .map(|captured| {
            captured
                .iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    Incoming {
        path,
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers: parts.headers,
        body,
    }
}

impl Incoming {
    /// A path parameter, which the router captured or did not, read by the row the
    /// description gave it.
    pub fn path_value(
        &self,
        name: &str,
        style: super::style::Style,
        explode: bool,
        array: bool,
        object: bool,
    ) -> Option<Value> {
        self.path
            .get(name)
            .map(|found| super::style::from_path(found, name, style, explode, array, object))
    }

    /// A query parameter, read by the row the description gave it.
    pub fn query_value(&self, name: &str, row: super::style::QueryRow) -> Option<Value> {
        super::style::from_query(&self.query, name, row)
    }

    /// A header, read by the row the description gave it: the raw text alone satisfied serde
    /// only for string scalars — an array-typed header the generated client itself writes as
    /// `a,b` was rejected 400 as "a string where a sequence was required".
    ///
    /// # Errors
    /// Returns [`RequestError::Header`] when the header is present and is not valid UTF-8.
    pub fn header_value(
        &self,
        name: &str,
        explode: bool,
        array: bool,
        object: bool,
    ) -> Result<Option<Value>, RequestError> {
        let Some(found) = self.headers.get(name) else {
            return Ok(None);
        };
        Ok(Some(super::style::from_header(
            found.to_str()?,
            explode,
            array,
            object,
        )))
    }

    /// One cookie out of the `Cookie` header, which carries all of them at once, read by the
    /// shape the description gave it and percent-decoded — the writing side encodes every
    /// element so caller data cannot spell a crumb boundary.
    ///
    /// Every `Cookie` field is searched: an HTTP/2 client may send each cookie in a field of its
    /// own (RFC 9113 §8.2.3).
    ///
    /// # Errors
    /// Returns [`RequestError::Header`] when a field is not valid UTF-8.
    pub fn cookie_value(
        &self,
        name: &str,
        array: bool,
        object: bool,
    ) -> Result<Option<Value>, RequestError> {
        for header in self.headers.get_all(::axum::http::header::COOKIE) {
            let found = header.to_str()?.split(';').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key.trim() == name).then(|| super::style::from_cookie(value.trim(), array, object))
            });
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// The base media type the request declares, when it declares one and it is not the one
    /// the description admits.
    ///
    /// Compared on the base type — parameters like `charset` and `boundary` never distinguish
    /// — and a request with *no* `Content-Type` passes: an absent header says nothing, and an
    /// optional body is often sent without one. What the comparison closes is the browser
    /// "simple request" hole: a cross-origin HTML form can post JSON-shaped text as
    /// `text/plain` with the victim's cookies and no preflight, and a JSON endpoint that
    /// parses whatever arrived is exactly the CSRF surface `axum::Json`'s own `415` exists to
    /// close.
    pub fn mismatched_media_type(&self, declared: &[&str]) -> Option<String> {
        let sent = self.headers.get(::axum::http::header::CONTENT_TYPE)?;
        // A header outside UTF-8 names no media type this server accepts.
        let Ok(sent) = sent.to_str() else {
            return Some("(unreadable)".to_owned());
        };
        let sent_base = base_media_type(sent);
        // A declared wildcard admits every type it matches: `image/*` any image, `*/*` anything.
        let admits = |declared: &&str| {
            let declared = base_media_type(declared);
            match declared.strip_suffix("/*") {
                Some("*") => true,
                Some(kind) => sent_base
                    .split_once('/')
                    .is_some_and(|(sent_kind, _)| sent_kind == kind),
                None => sent_base == declared,
            }
        };
        (!declared.iter().any(admits)).then_some(sent_base)
    }

    /// The whole body, or nothing when the request carried none.
    ///
    /// # Errors
    /// Returns [`RequestError::Body`] when the body could not be read off the connection.
    pub async fn bytes(self) -> Result<Option<Vec<u8>>, RequestError> {
        let collected = ::axum::body::to_bytes(self.body, BODY_LIMIT).await?;
        // An empty body and an absent body are the same thing over HTTP, and "absent" is the
        // reading that lets an optional body be optional.
        Ok((!collected.is_empty()).then(|| collected.to_vec()))
    }
}

/// A media type without its parameters, lowercased: what decides whether two spellings name
/// one type.
fn base_media_type(text: &str) -> String {
    text.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// How much of a body a generated server will hold in memory.
///
/// Two mebibytes, which is a decision rather than a default: a generated server must not be a
/// denial-of-service target because a description said `type: string`.
///
/// It is a **ceiling and not a default**, and `axum`'s `DefaultBodyLimit` does not raise it.
/// That layer works by inserting an extension which only extractors calling `with_limited_body`
/// consult; this reads the body with `to_bytes` and its own number, so the extension never comes
/// into it. A service needing larger bodies has to change this constant, which is a limitation
/// stated here rather than left to be discovered when a 3 MiB upload comes back as a 400.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// A JSON body as the type the description gave it.
///
/// # Errors
/// When the body cannot be read, or does not match the description.
pub async fn json_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
) -> Result<Option<T>, RequestError> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    Ok(Some(::serde_json::from_slice(&bytes)?))
}

/// A form-urlencoded body, read through the same style rows the query string uses.
///
/// # Errors
/// When the body cannot be read, or its members do not match the description.
///
/// `reading` is the plan the description gives the body's members.
pub async fn form_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::style::FormSpec],
    reading: &super::serve::Reading,
) -> Result<Option<T>, RequestError> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    let text = ::std::string::String::from_utf8_lossy(&bytes);
    // Through the same reading a query parameter gets: a form body carries no types either.
    Ok(Some(super::serve::typed(
        super::style::query_object(&text, specs),
        reading,
    )?))
}

/// A `multipart/form-data` body, read back into the type its parts came from.
///
/// # Errors
/// When the body cannot be read, is not well-formed multipart, or does not match the description.
///
/// `reading` is the plan the description gives the body's members, which keeps the values the
/// parser built for file and JSON parts.
pub async fn multipart_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::multipart::PartSpec],
    reading: &super::serve::Reading,
) -> Result<Option<T>, RequestError> {
    let content_type = incoming
        .headers
        .get(::axum::http::header::CONTENT_TYPE)
        .ok_or(RequestError::MissingMultipartBoundary)?
        .to_str()?;
    let boundary = super::multipart::boundary_of(content_type)
        .ok_or(RequestError::MissingMultipartBoundary)?;
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    let value = super::multipart::parse(&bytes, &boundary, specs)?;
    Ok(Some(super::serve::typed(value, reading)?))
}

/// A text body.
///
/// # Errors
/// When the body cannot be read, or is not valid UTF-8.
pub async fn text_body(incoming: Incoming) -> Result<Option<String>, RequestError> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    Ok(Some(::std::string::String::from_utf8(bytes)?))
}

/// A body the description said nothing about the shape of.
///
/// # Errors
/// When the body cannot be read off the connection.
pub async fn byte_body(incoming: Incoming) -> Result<Option<Vec<u8>>, RequestError> {
    incoming.bytes().await
}

/// The headers one response arm answers with, gathered before any of them is attached.
///
/// A generated arm writes its typed headers here and hands the result to one of the `respond_*`
/// writers, which attach it only once the body has encoded: a reply that fails on the way out is
/// a `500` that carries none of the arm's headers, so a failed `201` never sends its `Location`.
///
/// The raw headers are the handler's own and replace, name by name, whatever the typed fields
/// wrote, keeping every value and its order.
/// They are attached last, so a raw `Content-Type` replaces the arm's media type too.
#[derive(Debug)]
pub struct ResponseHeaders {
    typed: ::axum::http::HeaderMap,
    raw: ::axum::http::HeaderMap,
    /// Why a typed header could not be written, which makes the reply a `500`.
    defect: Option<String>,
}

impl ResponseHeaders {
    /// An arm's headers, starting from the ones the handler wrote as they are.
    #[must_use]
    pub fn new(raw: ::axum::http::HeaderMap) -> Self {
        Self {
            typed: ::axum::http::HeaderMap::new(),
            raw,
            defect: None,
        }
    }

    /// Write one declared header in its `simple` spelling, from its value as JSON.
    ///
    /// `list` is whether the description declares the header a list.
    /// A name the raw headers carry is theirs, so its typed value is neither written nor checked:
    /// a handler can send by hand what the typed field cannot hold.
    /// `null`, which is what a non-finite number serializes as, writes nothing.
    /// A scalar is written as its text, never split, and a list as its elements joined by commas
    /// in one field.
    ///
    /// A value that spelling cannot carry so that it reads back unchanged is the handler's defect
    /// and turns the reply into a `500`:
    ///
    /// - an object, or a list where the description declares one value;
    /// - a list holding anything but scalars, or a string element that is empty, holds a comma, or
    ///   starts or ends with a space or a tab, which a reader splitting the field on its commas
    ///   drops, splits or trims;
    /// - a string that starts or ends with a space or a tab, which HTTP strips from every field
    ///   value (RFC 9110, section 5.5), while an empty string is an empty field and reads back as
    ///   itself;
    /// - text HTTP does not allow in a field, such as a line break.
    pub fn typed(
        &mut self,
        name: &'static str,
        list: bool,
        value: Result<Value, ::serde_json::Error>,
    ) {
        if self.defect.is_some() || self.raw.contains_key(name) {
            return;
        }
        let refusal = match value {
            Err(err) => format!("could not be serialized: {err}"),
            Ok(Value::Null) => return,
            Ok(Value::Object(_)) => "is an object, which a header field cannot spell".to_owned(),
            // The reader of a header declared as one value never splits it, so a list would read
            // back as one string.
            Ok(Value::Array(_)) if !list => {
                "is a list, where the description declares one value".to_owned()
            }
            Ok(Value::Array(items)) if !items.iter().all(header_element) => {
                "is a list with an element its commas cannot keep apart from the others".to_owned()
            }
            Ok(Value::String(text)) if padded(&text) => {
                "starts or ends with whitespace, which HTTP strips from a field value".to_owned()
            }
            Ok(value) => {
                let text = super::style::header_value(&value, false);
                match (
                    ::axum::http::HeaderName::from_bytes(name.as_bytes()),
                    ::axum::http::HeaderValue::from_str(&text),
                ) {
                    (Ok(field), Ok(text)) => {
                        self.typed.insert(field, text);
                        return;
                    }
                    (Err(_), _) => "is not named by an HTTP field name".to_owned(),
                    (_, Err(_)) => "is not text an HTTP field can carry".to_owned(),
                }
            }
        };
        self.defect = Some(format!("the handler's `{name}` header {refusal}"));
    }

    /// The reply with these headers attached, or a `500` without them when one of them could
    /// not be written.
    fn attach(self, mut response: ::axum::response::Response) -> ::axum::response::Response {
        let Self {
            mut typed,
            raw,
            defect,
        } = self;
        if let Some(defect) = defect {
            return refused(&defect);
        }
        // `extend` replaces every value of a name it brings, in both steps.
        typed.extend(raw);
        response.headers_mut().extend(typed);
        response
    }
}

/// Whether a list element survives being joined with commas and split again: a reader drops an
/// empty element and trims the whitespace around each one (RFC 9110, section 5.6.1).
fn header_element(element: &Value) -> bool {
    match element {
        Value::String(text) => !text.is_empty() && !text.contains(',') && !padded(text),
        Value::Bool(_) | Value::Number(_) => true,
        Value::Null | Value::Array(_) | Value::Object(_) => false,
    }
}

/// Whether text starts or ends with the whitespace HTTP trims around a field value and a list
/// element.
fn padded(text: &str) -> bool {
    text.starts_with([' ', '\t']) || text.ends_with([' ', '\t'])
}

/// A reply this server could not write, which is its own defect rather than the caller's: a
/// `500` carrying the reason and none of the arm's headers.
fn refused(message: &str) -> ::axum::response::Response {
    use ::axum::response::IntoResponse as _;
    (
        ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        ::axum::Json(::serde_json::json!({ "message": message })),
    )
        .into_response()
}

/// The `500` refusing a status a handler answered with that HTTP has no code for.
fn unknown_status(status: u16) -> ::axum::response::Response {
    refused(&format!(
        "the handler answered with {status}, which is not an HTTP status"
    ))
}

/// One declared JSON arm's reply.
pub fn respond_json<T: ::serde::Serialize>(
    status: u16,
    content_type: &str,
    body: &T,
    headers: ResponseHeaders,
) -> ::axum::response::Response {
    use ::axum::response::IntoResponse as _;
    let Ok(code) = ::axum::http::StatusCode::from_u16(status) else {
        return unknown_status(status);
    };
    // A `204` carries no body by definition, whatever the arm declares, and still its headers.
    if code == ::axum::http::StatusCode::NO_CONTENT {
        return headers.attach(code.into_response());
    }
    // A JSON `null` is a body like any other: the arm declared JSON, and an empty reply would
    // say it declared nothing.
    match ::serde_json::to_value(body) {
        Ok(value) => {
            let mut response = (code, ::axum::Json(value)).into_response();
            // The declared spelling is the wire contract; `axum::Json` writes the plain
            // `application/json`, which is only the common case. A spelling that is not a
            // legal header value stays the plain one rather than aborting the response.
            let declared = match content_type {
                "application/json" => None,
                other => ::axum::http::HeaderValue::from_str(other).ok(),
            };
            if let Some(declared) = declared {
                response
                    .headers_mut()
                    .insert(::axum::http::header::CONTENT_TYPE, declared);
            }
            headers.attach(response)
        }
        // A response the handler built and serde cannot render is this server's own defect, not the
        // caller's, so it is a 500 rather than a rejection.
        Err(source) => refused(&source.to_string()),
    }
}

/// One declared arm's reply when the arm declares no content: the status and its headers alone.
pub fn respond_empty(status: u16, headers: ResponseHeaders) -> ::axum::response::Response {
    use ::axum::response::IntoResponse as _;
    match ::axum::http::StatusCode::from_u16(status) {
        Ok(code) => headers.attach(code.into_response()),
        Err(_) => unknown_status(status),
    }
}

/// One declared text arm's reply.
pub fn respond_text(
    status: u16,
    content_type: &'static str,
    body: ::std::string::String,
    headers: ResponseHeaders,
) -> ::axum::response::Response {
    respond_raw(status, content_type, body, headers)
}

/// One declared binary arm's reply.
pub fn respond_bytes<T>(
    status: u16,
    content_type: &'static str,
    body: T,
    headers: ResponseHeaders,
) -> ::axum::response::Response
where
    T: ::axum::response::IntoResponse,
{
    respond_raw(status, content_type, body, headers)
}

fn respond_raw<T>(
    status: u16,
    content_type: &'static str,
    body: T,
    headers: ResponseHeaders,
) -> ::axum::response::Response
where
    T: ::axum::response::IntoResponse,
{
    use ::axum::response::IntoResponse as _;

    let Ok(code) = ::axum::http::StatusCode::from_u16(status) else {
        return unknown_status(status);
    };
    if code == ::axum::http::StatusCode::NO_CONTENT {
        return headers.attach(code.into_response());
    }
    let Ok(content_type) = content_type.parse::<::axum::http::HeaderValue>() else {
        return refused("the declared response content type is not a valid HTTP header value");
    };
    let mut response = body.into_response();
    *response.status_mut() = code;
    response
        .headers_mut()
        .insert(::axum::http::header::CONTENT_TYPE, content_type);
    headers.attach(response)
}

impl ::axum::response::IntoResponse for super::serve::RejectionResponse {
    fn into_response(self) -> ::axum::response::Response {
        let (status, body) = self.into_parts();
        let code = ::axum::http::StatusCode::from_u16(status)
            .unwrap_or(::axum::http::StatusCode::BAD_REQUEST);
        (code, ::axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    /// An arm's headers when it declares none and the handler wrote none.
    fn no_headers() -> super::ResponseHeaders {
        super::ResponseHeaders::new(::axum::http::HeaderMap::new())
    }

    /// Every value of one header a response carries, in order.
    fn values(response: &::axum::response::Response, name: &str) -> Vec<String> {
        response
            .headers()
            .get_all(name)
            .iter()
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .collect()
    }

    /// The JSON body of a response the server wrote in full.
    ///
    /// Polled once, with no runtime: a body held in memory is ready on the first poll.
    fn json_body(response: ::axum::response::Response) -> eyre::Result<serde_json::Value> {
        use std::future::Future as _;

        let mut read = std::pin::pin!(::axum::body::to_bytes(response.into_body(), 1024));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(bytes) = read.as_mut().poll(&mut context) else {
            eyre::bail!("a body held in memory is ready at once");
        };
        Ok(serde_json::from_slice(&bytes?)?)
    }

    /// Typed headers in their `simple` spelling: a scalar whole, a list joined by commas.
    ///
    /// A field is declared a list exactly when its value is one, so a case that hands a list to
    /// a header declared as one value calls the writer itself.
    fn typed(
        raw: ::axum::http::HeaderMap,
        fields: &[(&'static str, serde_json::Value)],
    ) -> super::ResponseHeaders {
        let mut headers = super::ResponseHeaders::new(raw);
        for (name, value) in fields {
            headers.typed(name, value.is_array(), Ok(value.clone()));
        }
        headers
    }

    fn incoming_with(content_type: Option<&str>) -> super::Incoming {
        let mut headers = ::axum::http::HeaderMap::new();
        let header = content_type.and_then(|value| ::axum::http::HeaderValue::from_str(value).ok());
        if let Some(header) = header {
            headers.insert(::axum::http::header::CONTENT_TYPE, header);
        }
        super::Incoming {
            path: std::collections::BTreeMap::new(),
            query: String::new(),
            headers,
            body: ::axum::body::Body::empty(),
        }
    }

    /// An HTTP/2 client may send each cookie in a `Cookie` field of its own.
    #[test_util::test]
    fn a_cookie_is_read_from_whichever_field_carries_it() {
        let mut incoming = incoming_with(None);
        for field in ["theme=dark", "session=abc; lang=en"] {
            incoming.headers.append(
                ::axum::http::header::COOKIE,
                ::axum::http::HeaderValue::from_static(field),
            );
        }
        assert_eq!(
            incoming.cookie_value("session", false, false)?,
            Some(serde_json::Value::String("abc".to_owned()))
        );
        assert_eq!(
            incoming.cookie_value("theme", false, false)?,
            Some(serde_json::Value::String("dark".to_owned()))
        );
        assert_eq!(incoming.cookie_value("missing", false, false)?, None);
    }

    #[test_util::test]
    fn a_body_claiming_another_media_type_is_named_and_an_absent_claim_passes() {
        // The browser "simple request": JSON-shaped text posted as `text/plain` cross-origin,
        // no preflight. The declared type is the contract.
        assert_eq!(
            incoming_with(Some("text/plain")).mismatched_media_type(&["application/json"]),
            Some("text/plain".to_owned())
        );
        // Parameters never distinguish.
        assert_eq!(
            incoming_with(Some("application/json; charset=utf-8"))
                .mismatched_media_type(&["application/json"]),
            None
        );
        assert_eq!(
            incoming_with(Some("multipart/form-data; boundary=b"))
                .mismatched_media_type(&["multipart/form-data"]),
            None
        );
        // A declared wildcard admits every type it matches, and nothing else.
        assert_eq!(
            incoming_with(Some("image/png")).mismatched_media_type(&["image/*"]),
            None
        );
        assert_eq!(
            incoming_with(Some("text/plain")).mismatched_media_type(&["image/*"]),
            Some("text/plain".to_owned())
        );
        assert_eq!(
            incoming_with(Some("application/zip")).mismatched_media_type(&["*/*"]),
            None
        );
        // The type the generated client sends for a wildcard is admitted beside it.
        assert_eq!(
            incoming_with(Some("application/octet-stream"))
                .mismatched_media_type(&["image/*", "application/octet-stream"]),
            None
        );
        // No header says nothing — an optional body is often sent without one.
        assert_eq!(
            incoming_with(None).mismatched_media_type(&["application/json"]),
            None
        );
    }

    /// A JSON arm's `null` is sent as JSON; only an arm that declares no content, and a `204`,
    /// are sent without a body.
    #[test_util::test]
    fn a_json_null_is_a_body_and_an_empty_arm_is_not() {
        let content_type = |response: &::axum::response::Response| {
            response
                .headers()
                .get(::axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
        };
        let null = super::respond_json(200, "application/json", &None::<String>, no_headers());
        assert_eq!(content_type(&null).as_deref(), Some("application/json"));
        let empty = super::respond_empty(200, no_headers());
        assert_eq!(content_type(&empty), None);
        let no_content =
            super::respond_json(204, "application/json", &None::<String>, no_headers());
        assert_eq!(content_type(&no_content), None);
    }

    /// A typed header is written in its `simple` spelling, and a name the raw headers carry
    /// replaces it with every value they hold, in order; the other typed headers stay.
    #[test_util::test]
    fn the_raw_headers_replace_a_typed_field_of_the_same_name() {
        let mut raw = ::axum::http::HeaderMap::new();
        for value in ["soon", "later"] {
            raw.append("retry-after", ::axum::http::HeaderValue::from_static(value));
        }
        raw.append("x-trace", ::axum::http::HeaderValue::from_static("b"));
        raw.append("x-trace", ::axum::http::HeaderValue::from_static("a"));
        let headers = typed(
            raw,
            &[
                ("Retry-After", serde_json::json!(30)),
                ("Location", serde_json::json!("/pets/1, the first")),
                ("X-Ids", serde_json::json!([1, 2, 3])),
                ("X-Absent", serde_json::Value::Null),
            ],
        );
        let response = super::respond_empty(201, headers);
        assert_eq!(response.status(), 201);
        assert_eq!(values(&response, "retry-after"), ["soon", "later"]);
        assert_eq!(values(&response, "x-trace"), ["b", "a"]);
        // A scalar is never split on its commas, and a list is one field.
        assert_eq!(values(&response, "location"), ["/pets/1, the first"]);
        assert_eq!(values(&response, "x-ids"), ["1,2,3"]);
        // `null` writes nothing.
        assert_eq!(values(&response, "x-absent"), [] as [String; 0]);
    }

    /// The raw headers are attached after the body, so a raw `Content-Type` replaces the arm's
    /// declared media type: the deliberate escape hatch for a response that drifted.
    #[test_util::test]
    fn a_raw_content_type_replaces_the_declared_media_type() {
        let mut raw = ::axum::http::HeaderMap::new();
        raw.insert(
            ::axum::http::header::CONTENT_TYPE,
            ::axum::http::HeaderValue::from_static("application/vnd.pet+json"),
        );
        let json = super::respond_json(
            200,
            "application/problem+json",
            &42,
            typed(raw.clone(), &[]),
        );
        assert_eq!(values(&json, "content-type"), ["application/vnd.pet+json"]);
        let text = super::respond_text(200, "text/plain", "hi".to_owned(), typed(raw, &[]));
        assert_eq!(values(&text, "content-type"), ["application/vnd.pet+json"]);
    }

    /// A `204` drops the body an arm declares and keeps its headers, which may be all it says.
    #[test_util::test]
    fn a_header_only_reply_carries_its_headers() {
        let headers = typed(
            ::axum::http::HeaderMap::new(),
            &[("X-Outcome", serde_json::json!("none"))],
        );
        let response = super::respond_json(204, "application/json", &"dropped", headers);
        assert_eq!(response.status(), 204);
        assert_eq!(values(&response, "x-outcome"), ["none"]);
        assert_eq!(values(&response, "content-type"), [] as [String; 0]);
    }

    /// A body that cannot be encoded is the server's defect: a `500` that carries none of the
    /// arm's headers, typed or raw, so a failed `201` never sends its `Location`.
    #[test_util::test]
    fn a_body_that_fails_to_encode_answers_without_the_arm_headers() {
        /// A body whose serialization always fails, as one holding a map with non-string keys
        /// would.
        struct Unencodable;

        impl serde::Serialize for Unencodable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("refused"))
            }
        }

        let mut raw = ::axum::http::HeaderMap::new();
        raw.insert("x-trace", ::axum::http::HeaderValue::from_static("a"));
        let headers = typed(raw, &[("Location", serde_json::json!("/pets/1"))]);
        let response = super::respond_json(201, "application/json", &Unencodable, headers);
        assert_eq!(response.status(), 500);
        assert_eq!(values(&response, "location"), [] as [String; 0]);
        assert_eq!(values(&response, "x-trace"), [] as [String; 0]);
        assert_eq!(json_body(response)?["message"], "refused");
    }

    /// A typed value the `simple` spelling cannot carry so that it reads back unchanged is the
    /// handler's defect, answered `500` without any of the arm's headers rather than sent wrong.
    #[test_util::test]
    fn a_typed_header_that_cannot_be_written_answers_without_the_arm_headers() {
        let cases = [
            (
                serde_json::json!("line\nbreak"),
                "not text an HTTP field can carry",
            ),
            (serde_json::json!({"k": "v"}), "is an object"),
            (serde_json::json!(["a,b", "c"]), "commas"),
            (serde_json::json!([["nested"]]), "commas"),
        ];
        for (value, reason) in cases {
            let mut raw = ::axum::http::HeaderMap::new();
            raw.insert("x-trace", ::axum::http::HeaderValue::from_static("a"));
            let headers = typed(
                raw,
                &[
                    ("Location", serde_json::json!("/pets/1")),
                    ("X-Note", value.clone()),
                ],
            );
            let response = super::respond_empty(201, headers);
            assert_eq!(response.status(), 500, "{value}");
            assert_eq!(values(&response, "location"), [] as [String; 0], "{value}");
            assert_eq!(values(&response, "x-trace"), [] as [String; 0], "{value}");
            let body = json_body(response)?;
            let message = body["message"].as_str().unwrap_or_default();
            assert!(message.contains("`X-Note`"), "{message}");
            assert!(message.contains(reason), "{message}");
        }
    }

    /// A header declared as one value is read back whole, never split, so a list handed to it
    /// would arrive as one string: refused like any other value the spelling cannot carry, while
    /// the same list is written for a header the description declares a list.
    #[test_util::test]
    fn a_list_in_a_header_declared_as_one_value_answers_without_the_arm_headers() {
        let mut headers = super::ResponseHeaders::new(::axum::http::HeaderMap::new());
        headers.typed("X-Any", false, Ok(serde_json::json!([1, 2])));
        let response = super::respond_empty(201, headers);
        assert_eq!(response.status(), 500);
        assert_eq!(values(&response, "x-any"), [] as [String; 0]);
        let body = json_body(response)?;
        let message = body["message"].as_str().unwrap_or_default();
        assert!(message.contains("`X-Any` header is a list"), "{message}");

        let mut headers = super::ResponseHeaders::new(::axum::http::HeaderMap::new());
        headers.typed("X-Ids", true, Ok(serde_json::json!([1, 2])));
        let response = super::respond_empty(201, headers);
        assert_eq!(values(&response, "x-ids"), ["1,2"]);
    }

    /// A raw value replaces a typed one of the same name before the typed one is checked, so a
    /// typed value no field can carry, or one that fails to serialize, is no defect when the
    /// handler sends that name by hand; the name is matched in any case.
    #[test_util::test]
    fn a_raw_value_overrides_a_typed_one_that_cannot_be_written() {
        let mut raw = ::axum::http::HeaderMap::new();
        raw.insert(
            "location",
            ::axum::http::HeaderValue::from_static("/pets/1"),
        );
        raw.insert("x-count", ::axum::http::HeaderValue::from_static("many"));
        let mut headers = super::ResponseHeaders::new(raw);
        headers.typed("Location", false, Ok(serde_json::json!("bad\nvalue")));
        headers.typed("X-Count", false, Err(serde::ser::Error::custom("refused")));
        headers.typed("X-Kept", false, Ok(serde_json::json!("typed")));
        let response = super::respond_empty(201, headers);
        assert_eq!(response.status(), 201);
        assert_eq!(values(&response, "location"), ["/pets/1"]);
        assert_eq!(values(&response, "x-count"), ["many"]);
        assert_eq!(values(&response, "x-kept"), ["typed"]);
    }

    /// A value whose spelling a reader would change is refused rather than sent as another value:
    /// HTTP strips the whitespace around a field value, and a reader of a list drops its empty
    /// elements and trims each of the rest.
    /// An empty string and inner whitespace survive, so they are written.
    #[test_util::test]
    fn a_typed_value_a_reader_would_trim_or_drop_answers_without_the_arm_headers() {
        let refused = [
            (serde_json::json!(" leading"), "whitespace"),
            (serde_json::json!("trailing\t"), "whitespace"),
            (serde_json::json!(["", "a"]), "commas"),
            (serde_json::json!([" a", "b"]), "commas"),
            (serde_json::json!(["a", "b\t"]), "commas"),
        ];
        for (value, reason) in refused {
            let mut headers = super::ResponseHeaders::new(::axum::http::HeaderMap::new());
            headers.typed("X-Note", value.is_array(), Ok(value.clone()));
            let response = super::respond_empty(201, headers);
            assert_eq!(response.status(), 500, "{value}");
            assert_eq!(values(&response, "x-note"), [] as [String; 0], "{value}");
            let body = json_body(response)?;
            let message = body["message"].as_str().unwrap_or_default();
            assert!(message.contains(reason), "{value}: {message}");
        }
        let written = [
            (serde_json::json!(""), ""),
            (serde_json::json!("inner space"), "inner space"),
            (serde_json::json!(["a b", "c"]), "a b,c"),
        ];
        for (value, wire) in written {
            let mut headers = super::ResponseHeaders::new(::axum::http::HeaderMap::new());
            headers.typed("X-Note", value.is_array(), Ok(value.clone()));
            let response = super::respond_empty(201, headers);
            assert_eq!(response.status(), 201, "{value}");
            assert_eq!(values(&response, "x-note"), [wire], "{value}");
        }
    }

    /// A status HTTP has no code for is refused like any other handler defect.
    #[test_util::test]
    fn a_status_http_has_no_code_for_answers_without_the_arm_headers() {
        let headers = typed(
            ::axum::http::HeaderMap::new(),
            &[("Location", serde_json::json!("/pets/1"))],
        );
        let response = super::respond_empty(1000, headers);
        assert_eq!(response.status(), 500);
        assert_eq!(values(&response, "location"), [] as [String; 0]);
    }

    #[test_util::test]
    fn a_declared_json_spelling_is_the_content_type_the_response_carries() {
        let response = super::respond_json(200, "application/problem+json", &42, no_headers());
        assert_eq!(
            response
                .headers()
                .get(::axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        let plain = super::respond_json(200, "application/json", &42, no_headers());
        assert_eq!(
            plain
                .headers()
                .get(::axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
