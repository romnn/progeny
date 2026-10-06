//! The credentials a client holds for the `apiKey` security schemes its description sends in a
//! header, and the rule that decides which of them a request carries.
//!
//! Shipped into a generated crate only when its description declares such a scheme, so a client
//! without one carries none of this.
//! It names `reqwest`, so like the rest of the client runtime it compiles here under `cfg(test)`
//! against the dev-dependency.
//!
//! A request carries the credentials of the first alternative of its security requirement that
//! the client can meet in full, and nothing when it can meet none.
//! That is the description's own reading: alternatives are OR, the schemes within one are AND,
//! and an alternative met in part is not met.
//! A request the client has no credential for is still sent, without one: the server is the
//! authority on whether it needed one, and a caller may already supply the header some other
//! way — a default header on its `reqwest::Client`, or `header` on the request.

/// The credentials a client holds, one slot per header `apiKey` scheme of its description.
///
/// Slots are numbered the way the generated client numbers the schemes, which is also how a
/// generated request names the alternatives of its requirement.
/// Values are marked sensitive when stored, so the client's own `Debug` output does not spell
/// them.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Credentials {
    /// Each scheme's header name.
    headers: &'static [&'static str],
    values: ::std::vec::Vec<::std::option::Option<::reqwest::header::HeaderValue>>,
}

impl Credentials {
    /// No credential for any of `headers`.
    pub fn new(headers: &'static [&'static str]) -> Self {
        Self {
            headers,
            values: ::std::vec![::std::option::Option::None; headers.len()],
        }
    }

    /// Hold `value` for the scheme numbered `scheme`, replacing what was held.
    pub fn set(&mut self, scheme: usize, mut value: ::reqwest::header::HeaderValue) {
        value.set_sensitive(true);
        if let ::std::option::Option::Some(slot) = self.values.get_mut(scheme) {
            *slot = ::std::option::Option::Some(value);
        }
    }

    /// Send `request` carrying the credentials of the first alternative in `requirement` that
    /// this client holds every credential of.
    ///
    /// An alternative names schemes by number.
    /// An empty one — what an anonymous alternative, or one made only of schemes progeny does
    /// not send, becomes — is never chosen, because it would send nothing.
    /// A header the request already has keeps its value: a declared parameter of the same name
    /// and a header set on the request both outrank a held credential.
    ///
    /// # Errors
    ///
    /// Returns what `reqwest` returns for the request: an invalid header, method or URL, or a
    /// request that never completed.
    pub async fn send(
        &self,
        request: ::reqwest::RequestBuilder,
        requirement: &[&[usize]],
    ) -> ::std::result::Result<::reqwest::Response, ::reqwest::Error> {
        let (client, request) = request.build_split();
        let mut request = request?;
        self.apply(request.headers_mut(), requirement);
        client.execute(request).await
    }

    /// Add the chosen alternative's credentials to `headers`, leaving any header already there.
    fn apply(&self, headers: &mut ::reqwest::header::HeaderMap, requirement: &[&[usize]]) {
        let held = |scheme: &usize| {
            self.values
                .get(*scheme)
                .is_some_and(::std::option::Option::is_some)
        };
        let ::std::option::Option::Some(chosen) = requirement
            .iter()
            .find(|schemes| !schemes.is_empty() && schemes.iter().all(held))
        else {
            return;
        };
        for &scheme in *chosen {
            let (
                ::std::option::Option::Some(name),
                ::std::option::Option::Some(::std::option::Option::Some(value)),
            ) = (self.headers.get(scheme), self.values.get(scheme))
            else {
                continue;
            };
            // Checked when the client was generated; a name that still fails here is skipped
            // rather than allowed to panic in somebody's request path.
            let ::std::result::Result::Ok(name) =
                ::reqwest::header::HeaderName::from_bytes(name.as_bytes())
            else {
                continue;
            };
            if !headers.contains_key(&name) {
                headers.insert(name, value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use reqwest::header::{HeaderMap, HeaderValue};

    use super::Credentials;

    const HEADERS: &[&str] = &["X-Api-Key", "X-Tenant"];

    fn held(values: &[(usize, &'static str)]) -> Credentials {
        let mut credentials = Credentials::new(HEADERS);
        for &(scheme, value) in values {
            credentials.set(scheme, HeaderValue::from_static(value));
        }
        credentials
    }

    fn applied(credentials: &Credentials, requirement: &[&[usize]]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        credentials.apply(&mut headers, requirement);
        headers
    }

    /// The first alternative met in full wins; one met in part is passed over whole.
    #[test_util::test]
    fn the_first_alternative_met_in_full_is_sent() {
        let key_only = held(&[(0, "key")]);
        // `key AND tenant` is met in part, so the second alternative, `key`, is the one sent.
        let headers = applied(&key_only, &[&[0, 1], &[0]]);
        assert_eq!(
            headers.get("x-api-key").map(HeaderValue::as_bytes),
            Some(&b"key"[..])
        );
        assert!(!headers.contains_key("x-tenant"));

        // With both held, the combination is met and both headers go.
        let both = held(&[(0, "key"), (1, "tenant")]);
        let headers = applied(&both, &[&[0, 1], &[0]]);
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
        assert_eq!(applied(&key_only, &[&[], &[0]]).len(), 1);
        assert!(applied(&key_only, &[&[1]]).is_empty());
        assert!(applied(&held(&[]), &[&[0]]).is_empty());
        assert!(applied(&key_only, &[]).is_empty());
    }

    /// A header the request already carries outranks the held credential.
    #[test_util::test]
    fn a_header_already_set_keeps_its_value() {
        let key_only = held(&[(0, "key")]);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("explicit"));
        key_only.apply(&mut headers, &[&[0]]);
        assert_eq!(
            headers.get("x-api-key").map(HeaderValue::as_bytes),
            Some(&b"explicit"[..])
        );
        assert!(!format!("{key_only:?}").contains("\"key\""));
    }
}
