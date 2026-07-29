//! Walking the document's paths into operations.
//!
//! Three tolerance rules govern everything here, and they are the same rule seen from three
//! distances:
//!
//! * **A position degrades before an operation does.** A response body progeny cannot type becomes
//!   `serde_json::Value`; the operation stays. `miro` writes 612 references to a
//!   `components.responses` section it never declares, and skipping an operation per reference
//!   would delete most of that API over one missing member.
//! * **An operation degrades before it disappears.** An optional parameter with no defined
//!   serialization is omitted, because a caller who never sets it is unaffected.
//! * **An operation disappears only when calling it is impossible.** A *required* parameter with no
//!   defined serialization, or a path variable no parameter declares, leaves nothing that could be
//!   sent — and a method that cannot build its own request is worse than a method that is absent.
//!
//! One module per cluster: [`param`] what travels beside the body, [`body`] the one body and its
//! media type, [`response`] the status arms — and this file, which walks the paths and names what
//! it finds.

mod body;
mod param;
mod response;

use super::route::{self, PathTemplate};
use super::style::Location;
use super::{ApiModel, Method, OperationContract};
use crate::config::Config;
use crate::contract::{Contracts, Namer, RustIdent, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{Operation, PathItem};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::Docs;

/// Every operation the document declares, named and lowered.
pub(super) fn run(
    resolved: &ResolvedDocument,
    contracts: &Contracts,
    _config: &Config,
    ctx: &mut Ctx,
) -> ApiModel {
    let mut build = Build {
        resolved,
        contracts,
        namer: Namer::default(),
    };
    let mut operations = Vec::new();
    // Webhooks are deliberately absent: they are carried losslessly in the document model and are
    // not rendered in v1, and an operation the caller cannot call has no business in the catalogue
    // of operations the caller can call.
    let paths = resolved
        .document()
        .paths
        .as_ref()
        .map(|paths| &paths.items)
        .into_iter()
        .flatten();
    for (route, item) in paths {
        let Some(item) = resolved.path_item(item) else {
            continue;
        };
        build.path_item(item, route, &mut operations, ctx);
    }

    super::registrable::classify(&mut operations, ctx);
    ApiModel {
        operations,
        servers: Vec::new(),
    }
}

struct Build<'a> {
    resolved: &'a ResolvedDocument,
    contracts: &'a Contracts,
    /// Method names, kept unique across the whole client.
    namer: Namer,
}

impl Build<'_> {
    fn path_item(
        &mut self,
        item: &PathItem,
        route: &str,
        out: &mut Vec<OperationContract>,
        ctx: &mut Ctx,
    ) {
        let at = JsonPointer::root().child("paths").child(route);
        let template = match route::parse(route) {
            Ok(template) => template,
            Err(malformed) => {
                ctx.report(Diagnostic::new(
                    BreakageClass::UnregistrableRoute,
                    Action::Degrade,
                    at,
                    format!(
                        "{}; every operation on this path is skipped, because a request to it \
                         cannot be addressed",
                        malformed.detail
                    ),
                ));
                return;
            }
        };

        for (method, operation) in item.operations() {
            let at = at.child(method.slug());
            if let Some(built) = self.operation(operation, item, method, &template, route, &at, ctx)
            {
                out.push(built);
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an operation is the product of its document node, the path item above it, its \
                  method, its route and where it sits; threading them through a struct would put \
                  the same values one indirection further away"
    )]
    fn operation(
        &mut self,
        operation: &Operation,
        item: &PathItem,
        method: Method,
        template: &PathTemplate,
        route: &str,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<OperationContract> {
        let params = self.params(operation, item, at, ctx)?;

        // A template variable nothing declares leaves a hole in the URL. Checked after the
        // parameters, because path-level parameters count and only the merge knows all of them.
        let declared: Vec<String> = params
            .iter()
            .filter(|param| param.style.location() == Location::Path)
            .map(|param| param.wire_name.clone())
            .collect();
        let unbound = route::unbound(template, &declared);
        if !unbound.is_empty() {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                at.clone(),
                format!(
                    "the path names {} that no path parameter declares, so the URL cannot be \
                     built; the operation is skipped",
                    unbound
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            ));
            return None;
        }

        let rust_name = self.name(operation, method, route, at, ctx);
        // Body first, then responses: the order the struct literal used to evaluate them in, kept
        // so that hoisting the calls does not reorder their diagnostics under the fold cap.
        let body = self.body(operation, at, ctx);
        let mut responses = self.responses(operation, at, ctx);
        if method == Method::Head {
            // A `HEAD` response has no body on the wire — the transport strips it, whatever the
            // handler wrote — so the schemas these arms declare document the `GET` twin, not
            // anything this operation can receive. Decoding them is how the wire probe found this:
            // every `HEAD` in `jellyfin` failed on a body that HTTP guarantees is absent. The arms
            // and their statuses stay; only the payload type becomes what a bodiless response
            // actually carries.
            for arm in responses.arms.iter_mut().chain(responses.default.as_mut()) {
                arm.body = crate::api::ResponseBody::Empty;
            }
        }
        Some(OperationContract {
            rust_name,
            method,
            path: template.clone(),
            params,
            body,
            responses,
            docs: docs_of(operation),
            // Filled in once every operation exists: whether a route collides is a question about
            // the whole set, so it cannot be answered while the set is still being built.
            registrable: None,
            pagination: None,
            origin: at.clone(),
        })
    }

    /// The method name, kept unique across the client.
    ///
    /// An `operationId` when there is one, because that is the name the document chose and a
    /// reader of both will look for it. Otherwise the method and the route, which is deterministic
    /// and says where the call goes.
    fn name(
        &mut self,
        operation: &Operation,
        method: Method,
        route: &str,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> RustIdent {
        let declared = operation
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let wanted = if let Some(id) = declared {
            RustIdent::method(std::slice::from_ref(&id.to_owned()))
        } else {
            {
                let mut segments = vec![method.slug().to_owned()];
                segments.extend(
                    route
                        .split('/')
                        .filter(|segment| !segment.is_empty())
                        .map(|segment| segment.trim_matches(['{', '}']).to_owned()),
                );
                let synthesized = RustIdent::method(&segments);
                // The sentence names no identifier on purpose: a detail that varies per occurrence
                // cannot be aggregated, and most documents that omit one `operationId` omit every
                // one of them. The count is the finding; the names are in the generated source.
                ctx.report(Diagnostic::new(
                    BreakageClass::CollidingOperationId,
                    Action::Repair,
                    at.clone(),
                    "the operation declares no `operationId`, so its method is named after its \
                     method and path; adding one to the document pins the name",
                ));
                synthesized
            }
        };

        let unique = self.namer.unique(wanted.clone());
        if unique != wanted {
            ctx.report(Diagnostic::new(
                BreakageClass::CollidingOperationId,
                Action::Repair,
                at.clone(),
                format!(
                    "another operation is already called `{wanted}`, so this one is called \
                     `{unique}`; the two are told apart by a suffix because nothing in the \
                     document tells them apart"
                ),
            ));
        }
        unique
    }

    /// The type a schema at an API position became.
    fn type_at(&self, id: SchemaId) -> TypeRef {
        let key = crate::shape::key_of(self.resolved, id);
        self.contracts
            .type_of(&key)
            .cloned()
            .unwrap_or(TypeRef::Value)
    }
}

fn docs_of(operation: &Operation) -> Docs {
    Docs {
        title: operation.summary.clone(),
        description: operation.description.clone(),
        deprecated: operation.deprecated.unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::json;

    use crate::api::tests::{model_of, with_paths};

    #[test_util::test]
    fn an_operation_with_no_operation_id_is_named_after_its_method_and_path() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets/{petId}": {
                "get": {
                    "parameters": [{"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })))?;
        assert_eq!(model.operations()[0].rust_name.as_str(), "get_pets_pet_id");
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::CollidingOperationId)
        );
    }

    #[test_util::test]
    fn two_operations_that_want_one_name_are_told_apart_and_reported() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "list-pets", "responses": {"200": {"description": "ok"}}}},
            "/b": {"get": {"operationId": "list_pets", "responses": {"200": {"description": "ok"}}}},
        })))?;
        let names: Vec<&str> = model
            .operations()
            .iter()
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(names, ["list_pets", "list_pets2"]);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|found| found.class() == crate::BreakageClass::CollidingOperationId)
                .count(),
            1
        );
    }

    #[test_util::test]
    fn a_path_variable_nothing_declares_takes_its_operation_with_it() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets/{petId}": {"get": {"operationId": "getPet", "responses": {"200": {"description": "ok"}}}},
        })))?;
        assert!(model.operations().is_empty());
        let found = diagnostics
            .iter()
            .find(|found| found.class() == crate::BreakageClass::UnregistrableRoute)
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(found.detail().contains("petId"), "{found}");
    }
}
