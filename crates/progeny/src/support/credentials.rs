//! The credentials a client holds for the security schemes its description declares, and the
//! rule that decides which of them a request carries.
//!
//! Shipped into a generated crate only when its description declares a scheme progeny sends, so
//! a client without one carries none of this.
//! It names `reqwest`, so like the rest of the client runtime it compiles here under `cfg(test)`
//! against the dev-dependency.
//!
//! A request carries the credentials of the first alternative of its security requirement that
//! the client can meet in full, and nothing when it can meet none.
//! That is the description's own reading: alternatives are OR, the schemes within one are AND,
//! and an alternative met in part is not met.
//! A request the client has no credential for is still sent, without one: the server is the
//! authority on whether it needed one, and a caller may already supply the credential some other
//! way — a default header on its `reqwest::Client`, or `header` on the request.

/// Where one scheme's credential travels, as the generated client numbers the schemes.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// A header of this name.
    Header(&'static str),
    /// A query parameter of this name.
    Query(&'static str),
    /// A cookie of this name, merged into the `Cookie` header.
    Cookie(&'static str),
    /// The `Authorization` header, holding a complete `Bearer` or `Basic` value.
    Authorization,
}

/// One held credential: a header value, already marked sensitive, or the text of a query or
/// cookie key.
#[derive(Clone)]
enum Held {
    Header(::reqwest::header::HeaderValue),
    Text(::std::string::String),
}

/// Never the value: a client's `Debug` output is what ends up in logs.
impl ::std::fmt::Debug for Held {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Sensitive")
    }
}

/// The credentials a client holds, one slot per scheme of its description that progeny sends.
///
/// Slots are numbered the way the generated client numbers the schemes, which is also how a
/// generated request names the alternatives of its requirement.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Credentials {
    places: &'static [Place],
    values: ::std::vec::Vec<::std::option::Option<Held>>,
}

impl Credentials {
    /// No credential for any of `places`.
    pub fn new(places: &'static [Place]) -> Self {
        Self {
            places,
            values: ::std::vec![::std::option::Option::None; places.len()],
        }
    }

    /// Hold `value` as the header of the scheme numbered `scheme`, marked sensitive.
    pub fn set(&mut self, scheme: usize, mut value: ::reqwest::header::HeaderValue) {
        value.set_sensitive(true);
        self.hold(scheme, Held::Header(value));
    }

    /// Hold `value` as the text of a query or cookie key.
    pub fn set_text(&mut self, scheme: usize, value: ::std::string::String) {
        self.hold(scheme, Held::Text(value));
    }

    /// Hold `token` as `Bearer <token>`.
    pub fn set_bearer(&mut self, scheme: usize, token: &::reqwest::header::HeaderValue) {
        let mut value = b"Bearer ".to_vec();
        value.extend_from_slice(token.as_bytes());
        // A valid header value after a valid prefix is a valid header value, so this holds
        // whenever `token` is one; the branch only keeps a broken invariant from panicking.
        if let ::std::result::Result::Ok(value) = ::reqwest::header::HeaderValue::from_bytes(&value)
        {
            self.set(scheme, value);
        }
    }

    /// Hold `username` and `password` as `Basic <base64 of username:password>`.
    pub fn set_basic(&mut self, scheme: usize, username: &str, password: &str) {
        let encoded = base64_encode(::std::format!("{username}:{password}").as_bytes());
        // Base64 is ASCII, so the value is always a valid header value.
        if let ::std::result::Result::Ok(value) =
            ::reqwest::header::HeaderValue::from_str(&::std::format!("Basic {encoded}"))
        {
            self.set(scheme, value);
        }
    }

    fn hold(&mut self, scheme: usize, held: Held) {
        if let ::std::option::Option::Some(slot) = self.values.get_mut(scheme) {
            *slot = ::std::option::Option::Some(held);
        }
    }

    /// Send `request` carrying the credentials of the first alternative in `requirement` that
    /// this client holds every credential of.
    ///
    /// An alternative names schemes by number.
    /// An empty one — what an anonymous alternative, or one made only of schemes progeny does
    /// not send, becomes — is never chosen, because it would send nothing.
    /// What the request already carries keeps its value: a header, a query parameter or a cookie
    /// of the same name, whether a declared parameter or set on the request, outranks a held
    /// credential.
    ///
    /// # Errors
    ///
    /// Returns what `reqwest` returns for the request: an invalid header, method or URL, or a
    /// request that never completed.
    /// When a query key was added, the error's URL has its query removed, so the key does not
    /// reach the error's `Display` or `Debug` output.
    pub async fn send(
        &self,
        request: ::reqwest::RequestBuilder,
        requirement: &[&[usize]],
    ) -> ::std::result::Result<::reqwest::Response, ::reqwest::Error> {
        let (client, request) = request.build_split();
        let mut request = request?;
        let queried = self.apply(&mut request, requirement);
        client.execute(request).await.map_err(|mut error| {
            // Taken as one optional borrow rather than a nested `if`, which the edition 2021
            // the generated crates use could not collapse into a let chain.
            let url = if queried {
                error.url_mut()
            } else {
                ::std::option::Option::None
            };
            if let ::std::option::Option::Some(url) = url {
                url.set_query(::std::option::Option::None);
            }
            error
        })
    }

    /// Add the chosen alternative's credentials to `request`, leaving whatever it already
    /// carries under the same name.
    ///
    /// Returns whether a query key was added, which is what makes the URL secret.
    fn apply(&self, request: &mut ::reqwest::Request, requirement: &[&[usize]]) -> bool {
        let held = |scheme: &usize| {
            self.values
                .get(*scheme)
                .is_some_and(::std::option::Option::is_some)
        };
        let ::std::option::Option::Some(chosen) = requirement
            .iter()
            .find(|schemes| !schemes.is_empty() && schemes.iter().all(held))
        else {
            return false;
        };
        let mut queried = false;
        for &scheme in *chosen {
            let (
                ::std::option::Option::Some(place),
                ::std::option::Option::Some(::std::option::Option::Some(value)),
            ) = (self.places.get(scheme), self.values.get(scheme))
            else {
                continue;
            };
            match (place, value) {
                (Place::Header(name), Held::Header(value)) => {
                    // Checked when the client was generated; a name that still fails here is
                    // skipped rather than allowed to panic in somebody's request path.
                    if let ::std::result::Result::Ok(name) =
                        ::reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    {
                        insert_absent(request.headers_mut(), name, value);
                    }
                }
                (Place::Authorization, Held::Header(value)) => {
                    insert_absent(
                        request.headers_mut(),
                        ::reqwest::header::AUTHORIZATION,
                        value,
                    );
                }
                (Place::Query(name), Held::Text(value)) => {
                    let url = request.url_mut();
                    if !url.query_pairs().any(|(key, _)| key == *name) {
                        url.query_pairs_mut().append_pair(name, value);
                        queried = true;
                    }
                }
                (Place::Cookie(name), Held::Text(value)) => {
                    add_cookie(request.headers_mut(), name, value);
                }
                // A setter only ever holds the kind its place takes.
                _ => {}
            }
        }
        queried
    }
}

/// Insert `value` under `name` unless the headers already carry that name.
fn insert_absent(
    headers: &mut ::reqwest::header::HeaderMap,
    name: ::reqwest::header::HeaderName,
    value: &::reqwest::header::HeaderValue,
) {
    if !headers.contains_key(&name) {
        headers.insert(name, value.clone());
    }
}

/// Merge the cookie `name=value` into the `Cookie` header, unless a cookie of that name is
/// already there, and send the result sensitive.
///
/// One header carries every cookie, which is how RFC 6265 has a client send them: the merge joins
/// whatever `Cookie` lines the request already had with the key, rather than adding a second.
/// The value is percent-encoded outside the cookie-octet set, as a declared cookie parameter's
/// would be, so a key cannot spell a second cookie.
fn add_cookie(headers: &mut ::reqwest::header::HeaderMap, name: &str, value: &str) {
    let existing: ::std::vec::Vec<&[u8]> = headers
        .get_all(::reqwest::header::COOKIE)
        .iter()
        .map(::reqwest::header::HeaderValue::as_bytes)
        .collect();
    let taken = existing.iter().any(|line| {
        line.split(|&byte| byte == b';').any(|crumb| {
            let key = crumb.split(|&byte| byte == b'=').next().unwrap_or_default();
            key.trim_ascii() == name.as_bytes()
        })
    });
    if taken {
        return;
    }
    let mut line = existing.join(&b"; "[..]);
    if !line.is_empty() {
        line.extend_from_slice(b"; ");
    }
    line.extend_from_slice(name.as_bytes());
    line.push(b'=');
    for &byte in value.as_bytes() {
        // RFC 6265 `cookie-octet`, less `%`, which introduces an escape.
        if matches!(byte, 0x21 | 0x23..=0x24 | 0x26..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E)
        {
            line.push(byte);
        } else {
            line.extend_from_slice(::std::format!("%{byte:02X}").as_bytes());
        }
    }
    if let ::std::result::Result::Ok(mut merged) = ::reqwest::header::HeaderValue::from_bytes(&line)
    {
        merged.set_sensitive(true);
        headers.insert(::reqwest::header::COOKIE, merged);
    }
}

/// Standard base64 with padding, for the `Basic` scheme.
///
/// Written out because the generated crate depends on no codec, and the client's multipart
/// encoder ships only with a multipart body.
fn base64_encode(bytes: &[u8]) -> ::std::string::String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // Total by masking: every index into the alphabet is six bits.
    let glyph = |value: u32| {
        char::from(
            *ALPHABET
                .get(usize::try_from(value & 0b11_1111).unwrap_or_default())
                .unwrap_or(&b'A'),
        )
    };
    let mut out = ::std::string::String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(glyph(word >> (18 - 6 * index)));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{Credentials, Place};

    const PLACES: &[Place] = &[
        Place::Header("X-Api-Key"),
        Place::Header("X-Tenant"),
        Place::Query("api_key"),
        Place::Cookie("session"),
        Place::Authorization,
        Place::Authorization,
    ];

    fn held(values: &[(usize, &'static str)]) -> Credentials {
        let mut credentials = Credentials::new(PLACES);
        for &(scheme, value) in values {
            credentials.set(scheme, HeaderValue::from_static(value));
        }
        credentials
    }

    fn get(url: &str) -> eyre::Result<reqwest::Request> {
        Ok(reqwest::Request::new(reqwest::Method::GET, url.parse()?))
    }

    fn applied(credentials: &Credentials, requirement: &[&[usize]]) -> eyre::Result<HeaderMap> {
        let mut request = get("http://localhost/")?;
        credentials.apply(&mut request, requirement);
        Ok(request.headers().clone())
    }

    /// The first alternative met in full wins; one met in part is passed over whole.
    #[test_util::test]
    fn the_first_alternative_met_in_full_is_sent() {
        let key_only = held(&[(0, "key")]);
        // `key AND tenant` is met in part, so the second alternative, `key`, is the one sent.
        let headers = applied(&key_only, &[&[0, 1], &[0]])?;
        assert_eq!(
            headers.get("x-api-key").map(HeaderValue::as_bytes),
            Some(&b"key"[..])
        );
        assert!(!headers.contains_key("x-tenant"));

        // With both held, the combination is met and both headers go.
        let both = held(&[(0, "key"), (1, "tenant")]);
        let headers = applied(&both, &[&[0, 1], &[0]])?;
        assert_eq!(headers.len(), 2);
        assert!(
            headers
                .get("x-api-key")
                .is_some_and(HeaderValue::is_sensitive)
        );
    }

    /// An anonymous alternative does not stop a held credential from being sent, and a
    /// requirement nothing is held for sends nothing.
    #[test_util::test]
    fn an_unmet_requirement_sends_nothing() {
        let key_only = held(&[(0, "key")]);
        assert_eq!(applied(&key_only, &[&[], &[0]])?.len(), 1);
        assert!(applied(&key_only, &[&[1]])?.is_empty());
        assert!(applied(&held(&[]), &[&[0]])?.is_empty());
        assert!(applied(&key_only, &[])?.is_empty());
    }

    /// A header the request already carries outranks the held credential.
    #[test_util::test]
    fn a_header_already_set_keeps_its_value() {
        let key_only = held(&[(0, "key")]);
        let mut request = get("http://localhost/")?;
        request
            .headers_mut()
            .insert("x-api-key", HeaderValue::from_static("explicit"));
        key_only.apply(&mut request, &[&[0]]);
        assert_eq!(
            request
                .headers()
                .get("x-api-key")
                .map(HeaderValue::as_bytes),
            Some(&b"explicit"[..])
        );
        assert!(!format!("{key_only:?}").contains("\"key\""));
    }

    /// A query key joins the query string unless a parameter of its name is already there, and
    /// reports that the URL now holds a secret.
    #[test_util::test]
    fn a_query_key_is_appended_unless_its_name_is_taken() {
        let mut credentials = Credentials::new(PLACES);
        credentials.set_text(2, "s3cret value".to_owned());
        let mut request = get("http://localhost/pets?limit=2")?;
        assert!(credentials.apply(&mut request, &[&[2]]));
        assert_eq!(request.url().query(), Some("limit=2&api_key=s3cret+value"));

        let mut taken = get("http://localhost/pets?api_key=explicit")?;
        assert!(!credentials.apply(&mut taken, &[&[2]]));
        assert_eq!(taken.url().query(), Some("api_key=explicit"));
        // Nor does the text reach the client's own `Debug` output.
        assert!(!format!("{credentials:?}").contains("s3cret"));
    }

    /// A cookie key joins the cookies already in the header, encoded and sensitive, and a cookie
    /// of its name already there wins.
    #[test_util::test]
    fn a_cookie_key_is_merged_into_the_cookie_header() {
        let mut credentials = Credentials::new(PLACES);
        credentials.set_text(3, "a;b".to_owned());
        let mut request = get("http://localhost/")?;
        request
            .headers_mut()
            .insert("cookie", HeaderValue::from_static("theme=dark"));
        credentials.apply(&mut request, &[&[3]]);
        let cookie = request
            .headers()
            .get("cookie")
            .ok_or_else(|| eyre::eyre!("the cookie header is set"))?;
        assert_eq!(cookie.as_bytes(), b"theme=dark; session=a%3Bb");
        assert!(cookie.is_sensitive());

        let mut taken = get("http://localhost/")?;
        taken
            .headers_mut()
            .insert("cookie", HeaderValue::from_static("session=explicit"));
        credentials.apply(&mut taken, &[&[3]]);
        assert_eq!(
            taken.headers().get("cookie").map(HeaderValue::as_bytes),
            Some(&b"session=explicit"[..])
        );
    }

    /// A bearer token and a username and password become complete `Authorization` values.
    #[test_util::test]
    fn bearer_and_basic_credentials_are_authorization_values() {
        let mut credentials = Credentials::new(PLACES);
        credentials.set_bearer(4, &HeaderValue::from_static("t0ken"));
        credentials.set_basic(5, "Aladdin", "open sesame");
        let bearer = applied(&credentials, &[&[4]])?;
        assert_eq!(
            bearer.get("authorization").map(HeaderValue::as_bytes),
            Some(&b"Bearer t0ken"[..])
        );
        assert!(
            bearer
                .get("authorization")
                .is_some_and(HeaderValue::is_sensitive)
        );
        // The example RFC 7617 gives.
        let basic = applied(&credentials, &[&[5]])?;
        assert_eq!(
            basic.get("authorization").map(HeaderValue::as_bytes),
            Some(&b"Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="[..])
        );
    }

    /// Every padding case of the encoder, against RFC 4648's test vectors.
    #[test_util::test]
    fn the_encoder_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(super::base64_encode(plain.as_bytes()), encoded);
        }
    }
}
