//! The security schemes a client can send a credential for, and which of them each operation
//! requires.
//!
//! Every scheme progeny sends becomes one credential the client holds and one place it goes:
//!
//! - `apiKey` in a header, the query string or the cookie header, under the scheme's `name`;
//! - `http` with the `bearer` scheme, `oauth2` and `openIdConnect`: a token in
//!   `Authorization: Bearer`, because at request time an `OAuth2` or `OpenID Connect` token is a
//!   bearer token and obtaining one is the caller's business;
//! - `http` with the `basic` scheme: a username and password in `Authorization: Basic`.
//!
//! Anything else — another `http` scheme such as `digest`, `mutualTLS`, an unknown type — is
//! left to the caller's own `reqwest::Client`, and a requirement alternative naming one keeps
//! only the schemes progeny sends.
//!
//! What this module decides is data: the schemes, numbered, and per operation a list of
//! alternatives, each a list of scheme numbers.
//! The rule that picks one alternative at request time lives once, in the shipped
//! `support::credentials`, and a generated request holds only its static list.

use crate::contract::{Namer, RustIdent};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Operation, SecurityRequirement, SecurityScheme};
use crate::resolve::ResolvedDocument;

/// One scheme the client sends a credential for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialScheme {
    /// The scheme's key in `components.securitySchemes`, which requirements name it by.
    pub(crate) name: String,
    /// The client method that sets the credential, unique among the client's methods once
    /// [`name_setters`] has run.
    pub(crate) setter: RustIdent,
    /// Where the credential travels.
    pub(crate) place: Place,
    /// The scheme's declared `type`, for the setter's documentation.
    pub(crate) kind: String,
    /// The scopes an `oauth2` scheme's flows declare, documented on the setter and never
    /// checked: whether a token carries them is the authorization server's business.
    pub(crate) scopes: Vec<String>,
    pub(crate) description: Option<String>,
}

/// Where a credential travels, with the name it travels under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// A header of this name, carrying the value as given.
    Header(String),
    /// A query parameter of this name.
    Query(String),
    /// A cookie of this name, merged into the `Cookie` header.
    Cookie(String),
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// `Authorization: Basic <base64 of username:password>`.
    Basic,
}

impl Place {
    /// The header a declared header parameter would have to be named to carry this credential.
    pub(crate) fn header(&self) -> Option<&str> {
        match self {
            Self::Header(name) => Some(name),
            Self::Bearer | Self::Basic => Some("authorization"),
            Self::Query(_) | Self::Cookie(_) => None,
        }
    }
}

/// The schemes the document declares that the client can send, in key order — which is the
/// numbering requirements use.
///
/// An `apiKey` scheme whose name cannot be sent where it says — a header or cookie name that is
/// not a token, an empty query name — is left out and reported: it can be sent nowhere.
pub(crate) fn schemes(resolved: &ResolvedDocument, ctx: &mut Ctx) -> Vec<CredentialScheme> {
    let Some(declared) = resolved
        .document()
        .components
        .as_ref()
        .and_then(|components| components.security_schemes.as_ref())
    else {
        return Vec::new();
    };
    let mut schemes = Vec::new();
    for (name, node) in declared {
        let Some(scheme) = resolved.security_scheme(node) else {
            continue;
        };
        let Some(place) = place(name, scheme, ctx) else {
            continue;
        };
        schemes.push(CredentialScheme {
            name: name.clone(),
            setter: RustIdent::method(&["with".to_owned(), name.clone()]),
            place,
            kind: scheme.kind.clone().unwrap_or_default(),
            scopes: scopes(scheme),
            description: scheme.description.clone(),
        });
    }
    schemes
}

/// Where one scheme's credential goes, or `None` for a scheme the client does not send.
fn place(name: &str, scheme: &SecurityScheme, ctx: &mut Ctx) -> Option<Place> {
    match scheme.kind.as_deref()? {
        "apiKey" => {
            let wire = scheme.name.clone().unwrap_or_default();
            let (place, valid, what) = match scheme.location.as_deref()? {
                "header" => (Place::Header(wire.clone()), is_token(&wire), "header"),
                "cookie" => (Place::Cookie(wire.clone()), is_token(&wire), "cookie"),
                "query" => (
                    Place::Query(wire.clone()),
                    !wire.is_empty(),
                    "query parameter",
                ),
                _ => return None,
            };
            if !valid {
                ctx.report(Diagnostic::new(
                    BreakageClass::MalformedMember,
                    Action::Degrade,
                    JsonPointer::root()
                        .child("components")
                        .child("securitySchemes")
                        .child(name.to_owned())
                        .child("name"),
                    format!(
                        "the `apiKey` scheme names the {what} `{wire}`, which is not a valid \
                         {what} name; the client cannot send it, so the scheme is left out"
                    ),
                ));
                return None;
            }
            Some(place)
        }
        // The `scheme` member is a case-insensitive HTTP authentication scheme name.
        "http" => match scheme.scheme.as_deref()?.to_ascii_lowercase().as_str() {
            "bearer" => Some(Place::Bearer),
            "basic" => Some(Place::Basic),
            _ => None,
        },
        "oauth2" | "openIdConnect" => Some(Place::Bearer),
        _ => None,
    }
}

/// Every scope an `oauth2` scheme's flows declare, once each, in order.
fn scopes(scheme: &SecurityScheme) -> Vec<String> {
    let Some(flows) = &scheme.flows else {
        return Vec::new();
    };
    let mut scopes: Vec<String> = [
        &flows.implicit,
        &flows.password,
        &flows.client_credentials,
        &flows.authorization_code,
    ]
    .into_iter()
    .flatten()
    .flat_map(|flow| flow.scopes.iter().flatten().map(|(scope, _)| scope.clone()))
    .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// Make every setter name unique among the client's methods.
///
/// Run after every operation has claimed its method name, so an operation keeps the name its
/// `operationId` asked for and a setter is the one that yields.
pub(crate) fn name_setters(schemes: &mut [CredentialScheme], methods: &mut Namer) {
    // The constructor every client has, which no operation claims.
    methods.take(&RustIdent::method(&["with_client".to_owned()]));
    for scheme in schemes {
        scheme.setter = methods.unique(scheme.setter.clone());
    }
}

/// The alternatives an operation's credentials are chosen from, as scheme numbers.
///
/// The operation's own `security` when it declares one, the document's otherwise; an empty list
/// requires nothing.
/// An alternative keeps only the schemes in `schemes` and is dropped when that leaves it empty —
/// an anonymous alternative, or one made only of schemes the client does not send — because the
/// shipped rule never chooses an empty alternative.
pub(crate) fn requirement(
    operation: &Operation,
    document: Option<&Vec<SecurityRequirement>>,
    schemes: &[CredentialScheme],
) -> Vec<Vec<usize>> {
    if schemes.is_empty() {
        return Vec::new();
    }
    operation
        .security
        .as_ref()
        .or(document)
        .into_iter()
        .flatten()
        .filter_map(|alternative| {
            let numbered: Vec<usize> = alternative
                .keys()
                .filter_map(|name| schemes.iter().position(|scheme| scheme.name == *name))
                .collect();
            (!numbered.is_empty()).then_some(numbered)
        })
        .collect()
}

/// Whether `name` is an RFC 9110 token: one or more token characters, which is what a header
/// name and a cookie name both have to be.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::json;

    use super::Place;
    use crate::api::tests::model_of;

    /// The document's requirement applies where an operation declares none; an operation's own
    /// replaces it, an empty one requires nothing, and alternatives keep their order.
    #[test_util::test]
    fn each_operation_requires_its_effective_alternatives() {
        let (model, _) = model_of(json!({
            "openapi": "3.1.0",
            "security": [{"key": []}],
            "components": {"securitySchemes": {
                "key": {"type": "apiKey", "in": "header", "name": "X-Api-Key"},
                "tenant": {"type": "apiKey", "in": "header", "name": "X-Tenant"},
                "query": {"type": "apiKey", "in": "query", "name": "key"},
                "bearer": {"type": "http", "scheme": "bearer"},
            }},
            "paths": {
                "/inherited": {"get": {"operationId": "inherited",
                    "responses": {"204": {"description": "done"}}}},
                "/own": {"get": {"operationId": "own",
                    "security": [{"bearer": []}, {"key": [], "tenant": []}, {"query": []}, {}],
                    "responses": {"204": {"description": "done"}}}},
                "/open": {"get": {"operationId": "open", "security": [],
                    "responses": {"204": {"description": "done"}}}},
            },
        }))?;
        let names: Vec<(&str, &str)> = model
            .schemes()
            .iter()
            .map(|scheme| (scheme.name.as_str(), scheme.setter.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("bearer", "with_bearer"),
                ("key", "with_key"),
                ("query", "with_query"),
                ("tenant", "with_tenant"),
            ]
        );
        let required = |name: &str| {
            model
                .operations()
                .iter()
                .find(|operation| operation.rust_name.as_str() == name)
                .map(|operation| operation.security.clone())
        };
        assert_eq!(required("inherited"), Some(vec![vec![1]]));
        // Every alternative in order, and `{}`, which sends nothing, dropped.
        assert_eq!(required("own"), Some(vec![vec![0], vec![1, 3], vec![2]]));
        assert_eq!(required("open"), Some(vec![]));
    }

    /// A scheme setter yields to an operation that already took its name.
    #[test_util::test]
    fn a_setter_yields_to_an_operation_of_the_same_name() {
        let (model, _) = model_of(json!({
            "openapi": "3.1.0",
            "components": {"securitySchemes": {
                "key": {"type": "apiKey", "in": "header", "name": "X-Api-Key"},
            }},
            "paths": {"/key": {"get": {"operationId": "withKey", "security": [{"key": []}],
                "responses": {"204": {"description": "done"}}}}},
        }))?;
        let setter = model
            .schemes()
            .first()
            .map(|scheme| scheme.setter.as_str().to_owned());
        assert_eq!(setter.as_deref(), Some("with_key2"));
    }

    /// Each scheme type goes to its place, a type progeny does not send is left out silently, and
    /// an `oauth2` scheme's scopes are collected for its setter's documentation.
    #[test_util::test]
    fn each_scheme_type_has_its_place() {
        let (model, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "components": {"securitySchemes": {
                "basic": {"type": "http", "scheme": "Basic"},
                "bearer": {"type": "http", "scheme": "bearer"},
                "cookie": {"type": "apiKey", "in": "cookie", "name": "session"},
                "digest": {"type": "http", "scheme": "digest"},
                "oauth": {"type": "oauth2", "flows": {
                    "implicit": {"authorizationUrl": "https://example.invalid/a",
                                 "scopes": {"b": "", "a": ""}},
                    "password": {"tokenUrl": "https://example.invalid/t", "scopes": {"a": ""}}}},
                "oidc": {"type": "openIdConnect", "openIdConnectUrl": "https://example.invalid"},
                "query": {"type": "apiKey", "in": "query", "name": "api_key"},
                "tls": {"type": "mutualTLS"},
            }},
            "paths": {"/p": {"get": {"operationId": "p",
                "responses": {"204": {"description": "done"}}}}},
        }))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let places: Vec<(&str, &Place)> = model
            .schemes()
            .iter()
            .map(|scheme| (scheme.name.as_str(), &scheme.place))
            .collect();
        assert_eq!(
            places,
            [
                ("basic", &Place::Basic),
                ("bearer", &Place::Bearer),
                ("cookie", &Place::Cookie("session".to_owned())),
                ("oauth", &Place::Bearer),
                ("oidc", &Place::Bearer),
                ("query", &Place::Query("api_key".to_owned())),
            ]
        );
        let oauth = model
            .schemes()
            .iter()
            .find(|scheme| scheme.name == "oauth")
            .ok_or_eyre("the oauth2 scheme")?;
        assert_eq!(oauth.scopes, ["a", "b"]);
    }

    /// A header parameter named like a scheme's header is a credential, and a scheme that names
    /// no valid header is reported and left out.
    #[test_util::test]
    fn a_scheme_header_parameter_is_a_credential() {
        let (model, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "components": {"securitySchemes": {
                "key": {"type": "apiKey", "in": "header", "name": "X-Api-Key"},
                "broken": {"type": "apiKey", "in": "header", "name": "two words"},
            }},
            "paths": {"/key": {"get": {"operationId": "key",
                "parameters": [
                    {"name": "x-api-key", "in": "header", "schema": {"type": "string"}},
                    {"name": "X-Trace", "in": "header", "schema": {"type": "string"}},
                ],
                "responses": {"204": {"description": "done"}}}}},
        }))?;
        let credentials: Vec<(&str, bool)> = model
            .operations()
            .iter()
            .flat_map(|operation| &operation.params)
            .map(|param| (param.wire_name.as_str(), param.credential))
            .collect();
        assert_eq!(credentials, [("X-Trace", false), ("x-api-key", true)]);
        assert_eq!(model.schemes().len(), 1);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.location().to_string().ends_with("broken/name")),
            "{diagnostics:?}"
        );
    }
}
