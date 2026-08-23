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

/// Take a request apart.
pub async fn receive(request: ::axum::extract::Request) -> Incoming {
    use ::axum::extract::FromRequestParts as _;

    let (mut parts, body) = request.into_parts();
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
    pub fn query_value(
        &self,
        name: &str,
        style: super::style::Style,
        explode: bool,
        array: bool,
    ) -> Option<Value> {
        super::style::from_query(&self.query, name, style, explode, array)
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
    /// # Errors
    /// Returns [`RequestError::Header`] when the header is present and is not valid UTF-8.
    pub fn cookie_value(
        &self,
        name: &str,
        array: bool,
        object: bool,
    ) -> Result<Option<Value>, RequestError> {
        let Some(header) = self.headers.get(::axum::http::header::COOKIE) else {
            return Ok(None);
        };
        let header = header.to_str()?;
        Ok(header.split(';').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then(|| super::style::from_cookie(value.trim(), array, object))
        }))
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
    pub fn mismatched_media_type(&self, declared: &str) -> Option<String> {
        let sent = self.headers.get(::axum::http::header::CONTENT_TYPE)?;
        // A header outside UTF-8 names no media type this server accepts.
        let Ok(sent) = sent.to_str() else {
            return Some("(unreadable)".to_owned());
        };
        let sent_base = base_media_type(sent);
        (sent_base != base_media_type(declared)).then_some(sent_base)
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
pub async fn form_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::style::FormSpec],
) -> Result<Option<T>, RequestError> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    let text = ::std::string::String::from_utf8_lossy(&bytes);
    // Through the same reading a query parameter gets: a form body carries no types either.
    Ok(Some(super::serve::typed_body(super::style::query_object(
        &text, specs,
    ))?))
}

/// A `multipart/form-data` body, read back into the type its parts came from.
///
/// # Errors
/// When the body cannot be read, is not well-formed multipart, or does not match the description.
pub async fn multipart_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::multipart::PartSpec],
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
    Ok(Some(super::serve::typed_body(value)?))
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

/// One declared JSON arm's reply.
pub fn respond_json<T: ::serde::Serialize>(
    status: u16,
    content_type: &str,
    body: &T,
) -> ::axum::response::Response {
    use ::axum::response::IntoResponse as _;
    let code = ::axum::http::StatusCode::from_u16(status)
        .unwrap_or(::axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    // A `204` carries no body by definition, and `serde_json` renders a unit arm as `null`.
    // Sending `null` under a status that promises nothing is a body a client has to learn to
    // ignore, so it is not sent.
    if code == ::axum::http::StatusCode::NO_CONTENT {
        return code.into_response();
    }
    match ::serde_json::to_value(body) {
        Ok(Value::Null) => code.into_response(),
        Ok(value) => {
            let mut response = (code, ::axum::Json(value)).into_response();
            // The declared spelling is the wire contract; `axum::Json` writes the plain
            // `application/json`, which is only the common case. A spelling that is not a
            // legal header value stays the plain one rather than aborting the response.
            // Written without a let-chain: this file ships into generated crates, whose
            // manifests say edition 2021, where the chain does not parse.
            let declared = match content_type {
                "application/json" => None,
                other => ::axum::http::HeaderValue::from_str(other).ok(),
            };
            if let Some(declared) = declared {
                response
                    .headers_mut()
                    .insert(::axum::http::header::CONTENT_TYPE, declared);
            }
            response
        }
        // A response the handler built and serde cannot render is this server's own defect, not the
        // caller's, so it is a 500 rather than a rejection.
        Err(source) => (
            ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ::axum::Json(::serde_json::json!({ "message": source.to_string() })),
        )
            .into_response(),
    }
}

/// One declared text arm's reply.
pub fn respond_text(
    status: u16,
    content_type: &'static str,
    body: ::std::string::String,
) -> ::axum::response::Response {
    respond_raw(status, content_type, body)
}

/// One declared binary arm's reply.
pub fn respond_bytes<T>(
    status: u16,
    content_type: &'static str,
    body: T,
) -> ::axum::response::Response
where
    T: ::axum::response::IntoResponse,
{
    respond_raw(status, content_type, body)
}

fn respond_raw<T>(status: u16, content_type: &'static str, body: T) -> ::axum::response::Response
where
    T: ::axum::response::IntoResponse,
{
    use ::axum::response::IntoResponse as _;

    let code = ::axum::http::StatusCode::from_u16(status)
        .unwrap_or(::axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    if code == ::axum::http::StatusCode::NO_CONTENT {
        return code.into_response();
    }
    let Ok(content_type) = content_type.parse::<::axum::http::HeaderValue>() else {
        return (
            ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ::axum::Json(::serde_json::json!({
                "message": "the declared response content type is not a valid HTTP header value",
            })),
        )
            .into_response();
    };
    let mut response = body.into_response();
    *response.status_mut() = code;
    response
        .headers_mut()
        .insert(::axum::http::header::CONTENT_TYPE, content_type);
    response
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

    #[test_util::test]
    fn a_body_claiming_another_media_type_is_named_and_an_absent_claim_passes() {
        // The browser "simple request": JSON-shaped text posted as `text/plain` cross-origin,
        // no preflight. The declared type is the contract.
        assert_eq!(
            incoming_with(Some("text/plain")).mismatched_media_type("application/json"),
            Some("text/plain".to_owned())
        );
        // Parameters never distinguish.
        assert_eq!(
            incoming_with(Some("application/json; charset=utf-8"))
                .mismatched_media_type("application/json"),
            None
        );
        assert_eq!(
            incoming_with(Some("multipart/form-data; boundary=b"))
                .mismatched_media_type("multipart/form-data"),
            None
        );
        // No header says nothing — an optional body is often sent without one.
        assert_eq!(
            incoming_with(None).mismatched_media_type("application/json"),
            None
        );
    }

    #[test_util::test]
    fn a_declared_json_spelling_is_the_content_type_the_response_carries() {
        let response = super::respond_json(200, "application/problem+json", &42);
        assert_eq!(
            response
                .headers()
                .get(::axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        let plain = super::respond_json(200, "application/json", &42);
        assert_eq!(
            plain
                .headers()
                .get(::axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
