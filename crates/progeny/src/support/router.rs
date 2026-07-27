//! What a generated handler does with an `axum` request, and what it sends back.
//!
//! The half of the serving runtime that names `axum`, kept apart from [`super::serve`] for the same
//! reason the client runtime is kept apart from the style table: progeny does not depend on `axum`,
//! so this file is type-checked by the corpus compile gate on every generated crate rather than by
//! progeny's own build. The rules that can be tested here are, and they are next door.

use std::collections::BTreeMap;

use serde_json::Value;

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
    /// A path parameter, which the router captured or did not.
    ///
    /// # Errors
    /// Never; the signature matches the other three so one `read` serves all four locations.
    pub fn path_value(&self, name: &str) -> Result<Option<Value>, String> {
        Ok(self.path.get(name).map(|found| Value::String(found.clone())))
    }

    /// A query parameter, read by the row the description gave it.
    ///
    /// # Errors
    /// Never; an unreadable value is absent, and whether that costs anything is the caller's rule.
    pub fn query_value(
        &self,
        name: &str,
        style: super::style::Style,
        explode: bool,
    ) -> Result<Option<Value>, String> {
        Ok(super::style::from_query(&self.query, name, style, explode))
    }

    /// A header, as the text it carries.
    ///
    /// # Errors
    /// When the header is present and is not valid UTF-8, which is a request nothing can read.
    pub fn header_value(&self, name: &str) -> Result<Option<Value>, String> {
        let Some(found) = self.headers.get(name) else {
            return Ok(None);
        };
        found
            .to_str()
            .map(|text| Some(Value::String(text.to_owned())))
            .map_err(|source| source.to_string())
    }

    /// One cookie out of the `Cookie` header, which carries all of them at once.
    ///
    /// # Errors
    /// When the header is present and is not valid UTF-8.
    pub fn cookie_value(&self, name: &str) -> Result<Option<Value>, String> {
        let Some(header) = self.headers.get(::axum::http::header::COOKIE) else {
            return Ok(None);
        };
        let header = header.to_str().map_err(|source| source.to_string())?;
        Ok(header.split(';').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then(|| Value::String(value.trim().to_owned()))
        }))
    }

    /// The whole body, or nothing when the request carried none.
    ///
    /// # Errors
    /// When the body could not be read off the connection.
    pub async fn bytes(self) -> Result<Option<Vec<u8>>, String> {
        let collected = ::axum::body::to_bytes(self.body, BODY_LIMIT)
            .await
            .map_err(|source| source.to_string())?;
        // An empty body and an absent body are the same thing over HTTP, and "absent" is the
        // reading that lets an optional body be optional.
        Ok((!collected.is_empty()).then(|| collected.to_vec()))
    }
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
) -> Result<Option<T>, String> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    ::serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| source.to_string())
}

/// A form-urlencoded body, read through the same style rows the query string uses.
///
/// # Errors
/// When the body cannot be read, or its members do not match the description.
pub async fn form_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::style::FormSpec],
) -> Result<Option<T>, String> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    let text = ::std::string::String::from_utf8_lossy(&bytes);
    // Through the same reading a query parameter gets: a form body carries no types either.
    super::serve::typed_body(super::style::query_object(&text, specs)).map(Some)
}

/// A `multipart/form-data` body, read back into the type its parts came from.
///
/// # Errors
/// When the body cannot be read, is not well-formed multipart, or does not match the description.
pub async fn multipart_body<T: ::serde::de::DeserializeOwned>(
    incoming: Incoming,
    specs: &[super::multipart::PartSpec],
) -> Result<Option<T>, String> {
    let boundary = incoming
        .headers
        .get(::axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(super::multipart::boundary_of)
        .ok_or_else(|| "the request declares no multipart boundary".to_owned())?;
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    let value = super::multipart::parse(&bytes, &boundary, specs)?;
    super::serve::typed_body(value).map(Some)
}

/// A text body.
///
/// # Errors
/// When the body cannot be read, or is not valid UTF-8.
pub async fn text_body(incoming: Incoming) -> Result<Option<String>, String> {
    let Some(bytes) = incoming.bytes().await? else {
        return Ok(None);
    };
    ::std::string::String::from_utf8(bytes)
        .map(Some)
        .map_err(|source| source.to_string())
}

/// A body the description said nothing about the shape of.
///
/// # Errors
/// When the body cannot be read off the connection.
pub async fn byte_body(incoming: Incoming) -> Result<Option<Vec<u8>>, String> {
    incoming.bytes().await
}

/// One declared arm's reply.
pub fn respond<T: ::serde::Serialize>(status: u16, body: &T) -> ::axum::response::Response {
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
        Ok(value) => (code, ::axum::Json(value)).into_response(),
        // A response the handler built and serde cannot render is this server's own defect, not the
        // caller's, so it is a 500 rather than a rejection.
        Err(source) => (
            ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ::axum::Json(::serde_json::json!({ "message": source.to_string() })),
        )
            .into_response(),
    }
}

impl ::axum::response::IntoResponse for super::serve::RejectionResponse {
    fn into_response(self) -> ::axum::response::Response {
        let (status, body) = self.into_parts();
        let code = ::axum::http::StatusCode::from_u16(status)
            .unwrap_or(::axum::http::StatusCode::BAD_REQUEST);
        (code, ::axum::Json(body)).into_response()
    }
}
