//! Which half of the API a directional collapse costs.
//!
//! The type layer records the facts it cannot price ([`crate::contract::Collapse`]) and this is
//! where each is held against the directions its type actually travels. Two kinds today. An
//! access marker (`readOnly`/`writeOnly`) names the one direction a member goes, and the single
//! shared type serves both — which only costs something when the type crosses the direction the
//! marker excludes, so that is the only time it is reported. And a property that may be absent
//! *and* may be null becomes one `Option`, so "absent" and "present and null" stop being
//! different documents ([`crate::contract::Presence`]); what that costs depends on which
//! direction the type travels:
//!
//! * **In a request body** the caller loses the ability to *send* an explicit null. Where a `null`
//!   means "clear this field" and an absent member means "leave it alone" — the shape every PATCH
//!   endpoint has — that is the difference between two operations.
//! * **In a response body** the caller loses the ability to *tell* which one arrived. Usually
//!   harmless, occasionally not.
//! * **In both**, or in neither: a type reached from both directions pays both, and a type on no
//!   wire at all pays nothing a caller can observe.
//!
//! 27,044 fields across 58 documents, which is why the split is worth having rather than being a
//! nicety: a policy decision affecting that many fields deserves to know which half it affects.

use std::collections::{BTreeMap, BTreeSet};

use super::{ApiModel, BodyContract};
use crate::contract::{CollapseKind, Contracts, TypeIndex, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};

/// Which directions a type is reachable in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Reach {
    request: bool,
    response: bool,
}

impl Reach {
    /// How a diagnostic names this position.
    fn describe(self) -> &'static str {
        match (self.request, self.response) {
            (true, true) => "a request body and a response body",
            (true, false) => "a request body",
            (false, true) => "a response body",
            // A type no operation reaches: a component the document declares and never uses, which
            // is common and worth saying, because the collapse costs a caller nothing there.
            (false, false) => "no operation's body",
        }
    }

    /// What the collapse actually costs at this position.
    fn consequence(self) -> &'static str {
        match (self.request, self.response) {
            (true, _) => {
                "a caller cannot send an explicit null, which is how a request says `clear this` \
                 rather than `leave it alone`"
            }
            (false, true) => "a caller cannot tell an explicit null from an absent member",
            (false, false) => "nothing on the wire is affected, because no operation carries it",
        }
    }
}

/// Report every collapse, split by the position its type occupies.
pub(super) fn report(contracts: &Contracts, model: &ApiModel, ctx: &mut Ctx) {
    if contracts.collapses().is_empty() {
        return;
    }
    let reach = reachability(contracts, model);

    // Reported one by one and aggregated by the context, which folds records sharing a class and a
    // detail. The detail names the position, so the split falls out as one record per position
    // carrying its own count — rather than one record for the document, which is what made the
    // question unanswerable before.
    for collapse in contracts.collapses() {
        let at = reach.get(&collapse.owner).copied().unwrap_or_default();
        match collapse.kind {
            CollapseKind::Presence => ctx.report(Diagnostic::new(
                BreakageClass::PresenceCollapse,
                Action::Degrade,
                collapse.at.clone(),
                format!(
                    "the property may be absent and may be null, and the document says those are \
                     different; both become `None`. The type is reached from {}, so {}",
                    at.describe(),
                    at.consequence()
                ),
            )),
            // An access marker costs nothing until the type crosses the direction it excludes;
            // a `readOnly` member of a response-only type is exactly what the document said.
            CollapseKind::ReadOnly if at.request => ctx.report(Diagnostic::new(
                BreakageClass::AccessCollapse,
                Action::Degrade,
                collapse.at.clone(),
                "the property is `readOnly` — the document says only responses carry it — and \
                 its type is reached from a request body, so the one generated type carries it \
                 on the way out too",
            )),
            CollapseKind::WriteOnly if at.response => ctx.report(Diagnostic::new(
                BreakageClass::AccessCollapse,
                Action::Degrade,
                collapse.at.clone(),
                "the property is `writeOnly` — the document says only requests carry it — and \
                 its type is reached from a response body, so the one generated type carries it \
                 on the way back too",
            )),
            CollapseKind::ReadOnly | CollapseKind::WriteOnly => {}
        }
    }
}

/// Which types each direction reaches, transitively.
///
/// A body names one type, and that type names others; the collapse can be any distance down. The
/// walk is over `TypeRef::Named` edges, which is exactly the reachability a payload has.
fn reachability(contracts: &Contracts, model: &ApiModel) -> BTreeMap<TypeIndex, Reach> {
    let mut requests = BTreeSet::new();
    let mut responses = BTreeSet::new();
    for operation in model.operations() {
        // Every typed body is a request position, whatever its wire format: a multipart or form
        // body carries its type out with the call exactly as a JSON one does, and seeding JSON
        // alone once reported form-reached types as "no operation's body" — a false statement
        // about the wire.
        if let Some(ty) = operation.body.as_ref().and_then(BodyContract::ty) {
            seed(ty, &mut requests);
        }
        // A parameter is a request position too: its type travels out with the call.
        for param in &operation.params {
            seed(&param.ty, &mut requests);
        }
        for arm in operation
            .responses
            .arms
            .iter()
            .chain(&operation.responses.default)
        {
            if let Some(ty) = arm.body.json_type() {
                seed(ty, &mut responses);
            }
        }
    }

    let requests = close(contracts, requests);
    let responses = close(contracts, responses);
    let mut out: BTreeMap<TypeIndex, Reach> = BTreeMap::new();
    for index in requests {
        out.entry(index).or_default().request = true;
    }
    for index in responses {
        out.entry(index).or_default().response = true;
    }
    out
}

fn seed(ty: &TypeRef, out: &mut BTreeSet<TypeIndex>) {
    let mut reached = Vec::new();
    ty.named(&mut reached);
    out.extend(reached);
}

/// Everything reachable from a starting set, following the types those types name.
fn close(contracts: &Contracts, start: BTreeSet<TypeIndex>) -> BTreeSet<TypeIndex> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<TypeIndex> = start.into_iter().collect();
    while let Some(index) = queue.pop() {
        if !seen.insert(index) {
            continue;
        }
        let Some(contract) = contracts.get(index) else {
            continue;
        };
        let mut reached = Vec::new();
        for ty in contract.kind().references() {
            ty.named(&mut reached);
        }
        queue.extend(reached);
    }
    seen
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use serde_json::json;

    use crate::api::tests::model_of;

    /// A document with one request-side and one response-side type, each with a collapse.
    fn split_document() -> serde_json::Value {
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "post": {
                        "operationId": "createPet",
                        "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/PetPatch"}}}},
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}}},
                    },
                },
            },
            "components": {"schemas": {
                // Distinct field names, so the two do not merge into one type.
                "PetPatch": {"type": "object", "properties": {"nickname": {"type": ["string", "null"]}}},
                "Pet": {"type": "object", "properties": {"owner": {"type": ["string", "null"]}}},
                "Unused": {"type": "object", "properties": {"note": {"type": ["string", "null"]}}},
            }},
        })
    }

    #[test_util::test]
    fn a_collapse_says_which_half_of_the_api_it_costs() {
        let (_, diagnostics) = model_of(split_document())?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::PresenceCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(
            found
                .iter()
                .any(|detail| detail.contains("a request body") && detail.contains("clear this")),
            "{found:#?}"
        );
        assert!(
            found.iter().any(
                |detail| detail.contains("a response body") && detail.contains("absent member")
            ),
            "{found:#?}"
        );
        // The component nothing calls: still reported, and honest about costing nothing.
        assert!(
            found
                .iter()
                .any(|detail| detail.contains("no operation's body")),
            "{found:#?}"
        );
    }

    /// An access marker is reported exactly where it costs something.
    ///
    /// `readOnly` says only responses carry the member; a type that crosses a request body
    /// carries it anyway, because there is one generated type for both directions. A `readOnly`
    /// member of a response-only type is exactly what the document said, so it stays silent —
    /// these markers were dropped with no record at all until this diagnostic existed.
    #[test_util::test]
    fn an_access_marker_reports_only_where_the_type_crosses_it() {
        let (_, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "post": {
                        "operationId": "createPet",
                        "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/PetIn"}}}},
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/PetOut"}}}}},
                    },
                },
            },
            "components": {"schemas": {
                // Crosses a request body carrying a `readOnly` member: reported.
                "PetIn": {"type": "object", "properties": {"id": {"type": "integer", "readOnly": true}, "in_only": {"type": "string"}}},
                // A `readOnly` member that stays on the response side: silent.
                "PetOut": {"type": "object", "properties": {"id": {"type": "integer", "readOnly": true}, "out_only": {"type": "string"}}},
            }},
        }))?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::AccessCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("`readOnly`"), "{found:#?}");
        assert!(found[0].contains("request body"), "{found:#?}");
    }

    /// The mirror image: `writeOnly` costs something on the way back.
    #[test_util::test]
    fn a_write_only_member_of_a_response_is_reported() {
        let (_, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "getPet",
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}}},
                    },
                },
            },
            "components": {"schemas": {
                "Pet": {"type": "object", "properties": {"secret": {"type": "string", "writeOnly": true}}},
            }},
        }))?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::AccessCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("`writeOnly`"), "{found:#?}");
        assert!(found[0].contains("response body"), "{found:#?}");
    }

    /// A multipart body is a request position like a JSON one.
    ///
    /// Seeding reachability from JSON bodies alone reported a form-reached type as "no
    /// operation's body" — a false statement about the wire, in a diagnostic the caller is meant
    /// to act on. The corpus has 278 multipart bodies across 27 documents.
    #[test_util::test]
    fn a_multipart_body_is_a_request_position() {
        let (_, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "post": {
                        "operationId": "createPet",
                        "requestBody": {"content": {"multipart/form-data": {"schema": {"$ref": "#/components/schemas/PetForm"}}}},
                        "responses": {"204": {"description": "done"}},
                    },
                },
            },
            "components": {"schemas": {
                "PetForm": {"type": "object", "properties": {"nickname": {"type": ["string", "null"]}}},
            }},
        }))?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::PresenceCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("a request body"), "{found:#?}");
    }

    #[test_util::test]
    fn a_type_reached_from_both_directions_pays_both() {
        let (_, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "post": {
                        "operationId": "createPet",
                        "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}},
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Pet"}}}}},
                    },
                },
            },
            "components": {"schemas": {
                "Pet": {"type": "object", "properties": {"owner": {"type": ["string", "null"]}}},
            }},
        }))?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::PresenceCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].contains("a request body and a response body"),
            "{found:#?}"
        );
    }

    #[test_util::test]
    fn a_collapse_deep_inside_a_body_is_still_attributed_to_the_position() {
        let (_, diagnostics) = model_of(json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Page"}}}}},
                    },
                },
            },
            "components": {"schemas": {
                "Page": {"type": "object", "properties": {"items": {"type": "array", "items": {"$ref": "#/components/schemas/Pet"}}}},
                "Pet": {"type": "object", "properties": {"owner": {"type": ["string", "null"]}}},
            }},
        }))?;
        let found: Vec<&str> = diagnostics
            .iter()
            .filter(|found| found.class() == crate::BreakageClass::PresenceCollapse)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        // Two hops from the response body — through a list — and still a response position.
        assert!(found[0].contains("a response body"), "{found:#?}");
    }
}
