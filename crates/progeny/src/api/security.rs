//! The `apiKey` security schemes a client sends in a header, and which of them each operation
//! requires.
//!
//! Only `apiKey` with `in: header` is modelled.
//! A key `in: query` lands in the URL, where no marking keeps it out of logs, and one
//! `in: cookie` shares the cookie header with the operation's cookie parameters; `http` (bearer
//! and basic), `oauth2` and `openIdConnect` all need a token lifecycle the caller's own
//! `reqwest::Client` already handles.
//! A requirement alternative naming any of those keeps only its header `apiKey` schemes: the
//! rest is the caller's to supply, as it is for a description with no scheme progeny sends.
//!
//! What this module decides is data: the schemes, numbered, and per operation a list of
//! alternatives, each a list of scheme numbers.
//! The rule that picks one alternative at request time lives once, in the shipped
//! `support::credentials`, and a generated request holds only its static list.

use crate::contract::{Namer, RustIdent};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Operation, SecurityRequirement};
use crate::resolve::ResolvedDocument;

/// One `apiKey` scheme sent in a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialScheme {
    /// The scheme's key in `components.securitySchemes`, which requirements name it by.
    pub(crate) name: String,
    /// The client method that sets the credential, unique among the client's methods once
    /// [`name_setters`] has run.
    pub(crate) setter: RustIdent,
    /// The header the credential travels in, as the document spells it.
    pub(crate) header: String,
    pub(crate) description: Option<String>,
}

/// The header `apiKey` schemes the document declares, in key order — which is the numbering
/// requirements use.
///
/// A scheme whose header name is missing or is not a valid header name is left out and
/// reported: it can be sent nowhere.
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
        if scheme.kind.as_deref() != Some("apiKey") || scheme.location.as_deref() != Some("header")
        {
            continue;
        }
        let header = scheme.name.clone().unwrap_or_default();
        if !is_header_name(&header) {
            ctx.report(Diagnostic::new(
                BreakageClass::MalformedMember,
                Action::Degrade,
                JsonPointer::root()
                    .child("components")
                    .child("securitySchemes")
                    .child(name.clone())
                    .child("name"),
                format!(
                    "the `apiKey` scheme names the header `{header}`, which is not a valid \
                     header name; the client cannot send it, so the scheme is left out"
                ),
            ));
            continue;
        }
        schemes.push(CredentialScheme {
            name: name.clone(),
            setter: RustIdent::method(&["with".to_owned(), name.clone()]),
            header,
            description: scheme.description.clone(),
        });
    }
    schemes
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

/// Whether `name` is an RFC 9110 field name: one or more token characters.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::json;

    use crate::api::tests::model_of;

    /// The document's requirement applies where an operation declares none; an operation's own
    /// replaces it, an empty one requires nothing, and alternatives keep their order with the
    /// schemes the client does not send left out.
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
        assert_eq!(names, [("key", "with_key"), ("tenant", "with_tenant")]);
        let required = |name: &str| {
            model
                .operations()
                .iter()
                .find(|operation| operation.rust_name.as_str() == name)
                .map(|operation| operation.security.clone())
        };
        assert_eq!(required("inherited"), Some(vec![vec![0]]));
        // `bearer` and `query` are the caller's business, and `{}` sends nothing.
        assert_eq!(required("own"), Some(vec![vec![0, 1]]));
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
