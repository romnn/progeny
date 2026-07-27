//! Declared pagination, resolved against the document that has to support it.
//!
//! **Never detected.** 62 of the corpus's 78 documents paginate and no two agree on how to say so:
//! the cursor parameter is called `offset` 541 times, `page` 319, `cursor` 213, `after` 198, and on
//! through `page_token`, `page[cursor]`, `PageToken` and `start`. The response side is the same —
//! `next`, `next_page`, `has_more`, `next_cursor`, `NextToken`, and a `Hasmore` that is somebody's
//! typo. Detection would be a table of vendor spellings pretending to be a rule, which is what the
//! predecessor did and what did not generalize to this corpus.
//!
//! So the caller declares it, and **every name in the declaration is checked against the document**
//! before anything is generated. A cursor parameter that no operation has, a member path that does
//! not resolve, an `items` path that is not a list — each is a hard error naming what it looked for
//! and what it found. The caller stated an intent about one named operation; generating a method
//! that cannot work, or quietly skipping it, would both be worse than refusing.

use crate::config::{Config, Pagination};
use crate::contract::{ContractKind, Contracts, RustIdent, TypeRef};
use crate::diag::{JsonPointer, RejectError, RejectKind};

use super::{Location, OperationContract, StatusPattern};

/// One operation's validated pagination, in the terms the renderer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaginationContract {
    /// The cursor parameter's Rust name, for the builder setter the stream drives.
    pub(crate) cursor_param: RustIdent,
    /// Member path to the next cursor, resolved to Rust names.
    pub(crate) next_cursor: Vec<RustIdent>,
    /// Member path to the page's items, resolved to Rust names.
    pub(crate) items: Vec<RustIdent>,
    /// The element type of `items`, which is what the stream yields.
    pub(crate) item: TypeRef,
    /// The variant of the response enum the stream reads, and its type.
    pub(crate) success: RustIdent,
}

/// Attach every declaration to its operation, or refuse to generate.
pub(crate) fn attach(
    operations: &mut [OperationContract],
    contracts: &Contracts,
    config: &Config,
) -> Result<(), RejectError> {
    for (name, declared) in &config.pagination {
        // By generated method name or by the operation's position in the document, which is the
        // same pair of keys `names` and `type-derives` accept for types.
        let Some(operation) = operations.iter_mut().find(|operation| {
            operation.rust_name.as_str() == name || operation.origin.to_string() == *name
        }) else {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the configuration declares pagination for `{name}`, which is not an operation \
                     this document has"
                ),
            ));
        };
        let resolved = resolve(operation, declared, contracts)?;
        operation.pagination = Some(resolved);
    }
    Ok(())
}

fn resolve(
    operation: &OperationContract,
    declared: &Pagination,
    contracts: &Contracts,
) -> Result<PaginationContract, RejectError> {
    let at = operation.origin.clone();
    let cursor = operation
        .params_at(Location::Query)
        .find(|param| param.wire_name == declared.cursor_param)
        .ok_or_else(|| {
            let had: Vec<&str> = operation
                .params_at(Location::Query)
                .map(|param| param.wire_name.as_str())
                .collect();
            reject(
                &at,
                format!(
                    "`{}` declares its cursor as the query parameter `{}`, which it does not have; \
                     it has {}",
                    operation.rust_name,
                    declared.cursor_param,
                    if had.is_empty() {
                        "no query parameters".to_owned()
                    } else {
                        format!("`{}`", had.join("`, `"))
                    }
                ),
            )
        })?;

    let (success, response) = success_arm(operation).ok_or_else(|| {
        reject(
            &at,
            format!(
                "`{}` declares pagination and has no success response to read a page out of",
                operation.rust_name
            ),
        )
    })?;

    let (items, item_ty) = walk(
        &response,
        &declared.items,
        contracts,
        &at,
        operation,
        "items",
    )?;
    let item = match strip(&item_ty) {
        TypeRef::Vec(inner) => strip(&inner),
        other => {
            return Err(reject(
                &at,
                format!(
                    "`{}` declares its items at `{}`, which is {} rather than a list, so there is \
                     nothing for the stream to yield",
                    operation.rust_name,
                    declared.items,
                    describe(&other)
                ),
            ));
        }
    };

    let (next_cursor, cursor_ty) = walk(
        &response,
        &declared.next_cursor,
        contracts,
        &at,
        operation,
        "next-cursor",
    )?;
    // An absent next cursor is the only thing that ends the stream, so a member that is always
    // present would loop forever. Required rather than worked around: the alternatives — stopping
    // on an empty string, on an empty page — are conventions the declaration did not state and this
    // has no business assuming.
    if !matches!(cursor_ty, TypeRef::Option(_)) {
        return Err(reject(
            &at,
            format!(
                "`{}` reads its next cursor from `{}`, which is always present; the stream ends \
                 when the service stops sending one, so that member has to be optional",
                operation.rust_name, declared.next_cursor
            ),
        ));
    }
    // The cursor goes back out through the parameter it came from, so the two have to be the same
    // shape. Checked rather than coerced: a cursor read as a number and sent as a string is the
    // kind of mismatch that only shows up against the live service.
    if strip(&cursor_ty) != strip(&cursor.ty) {
        return Err(reject(
            &at,
            format!(
                "`{}` reads its next cursor from `{}`, which is {}, and sends it as `{}`, which is \
                 {}; the stream would have to invent a conversion",
                operation.rust_name,
                declared.next_cursor,
                describe(&strip(&cursor_ty)),
                declared.cursor_param,
                describe(&strip(&cursor.ty))
            ),
        ));
    }

    Ok(PaginationContract {
        cursor_param: cursor.rust_name.clone(),
        next_cursor,
        items,
        item,
        success,
    })
}

/// The one arm answering a 2xx, which is the page the stream reads.
///
/// Exactly one, not the first of several. When a document declares two success statuses the client
/// hands back an enum, and a stream would have to decide which variant carries a page — a decision
/// the document did not make and this is in no position to invent.
fn success_arm(operation: &OperationContract) -> Option<(RustIdent, TypeRef)> {
    let mut successes = operation.responses.arms.iter().filter(
        |arm| matches!(arm.status, StatusPattern::Exact(code) if (200..300).contains(&code)),
    );
    let only = successes.next()?;
    successes
        .next()
        .is_none()
        .then(|| (only.rust_name.clone(), only.ty.clone()))
}

/// Follow a dotted path of wire names through named struct types.
fn walk(
    from: &TypeRef,
    path: &str,
    contracts: &Contracts,
    at: &JsonPointer,
    operation: &OperationContract,
    what: &str,
) -> Result<(Vec<RustIdent>, TypeRef), RejectError> {
    let mut current = from.clone();
    let mut resolved = Vec::new();
    for (depth, segment) in path.split('.').enumerate() {
        // An optional member *inside* a path would make the generated access a question rather than
        // a field read, and the answer — skip the page, end the stream, treat it as empty — is not
        // one the declaration states. The last member may be optional: a next cursor that is absent
        // is exactly how a service says "this was the last page".
        if depth > 0 && matches!(current, TypeRef::Option(_)) {
            return Err(reject(
                at,
                format!(
                    "`{}` declares its {what} at `{path}`, and the member before `{segment}` is \
                     optional; a path through an absent member has no reading this can pick for you",
                    operation.rust_name
                ),
            ));
        }
        let TypeRef::Named(index) = strip(&current) else {
            return Err(reject(
                at,
                format!(
                    "`{}` declares its {what} at `{path}`, and `{segment}` is being looked for in \
                     {}, which has no members",
                    operation.rust_name,
                    describe(&strip(&current))
                ),
            ));
        };
        let Some(ContractKind::Struct { fields }) = contracts
            .get(index)
            .map(crate::contract::TypeContract::kind)
        else {
            return Err(reject(
                at,
                format!(
                    "`{}` declares its {what} at `{path}`, and `{segment}` is being looked for in a \
                     type that is not a struct",
                    operation.rust_name
                ),
            ));
        };
        let Some(field) = fields.iter().find(|field| field.wire_name == segment) else {
            let had: Vec<&str> = fields
                .iter()
                .map(|field| field.wire_name.as_str())
                .collect();
            return Err(reject(
                at,
                format!(
                    "`{}` declares its {what} at `{path}`, and no member of that type is called \
                     `{segment}`; it has `{}`",
                    operation.rust_name,
                    had.join("`, `")
                ),
            ));
        };
        resolved.push(field.rust_name.clone());
        current = field.ty.clone();
    }
    Ok((resolved, current))
}

/// See through the wrappers that do not change what a value *is*.
fn strip(ty: &TypeRef) -> TypeRef {
    match ty {
        TypeRef::Option(inner) | TypeRef::Boxed(inner) => strip(inner),
        other => other.clone(),
    }
}

/// What a type is, in words a configuration error can use.
fn describe(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Named(_) => "a named type".to_owned(),
        TypeRef::Vec(inner) => format!("a list of {}", describe(inner)),
        TypeRef::Map(_) => "a map".to_owned(),
        TypeRef::String => "a string".to_owned(),
        TypeRef::Bool => "a boolean".to_owned(),
        TypeRef::I64 | TypeRef::U64 | TypeRef::F64 => "a number".to_owned(),
        TypeRef::Value => "arbitrary JSON".to_owned(),
        TypeRef::Unit => "nothing".to_owned(),
        other => format!("{other:?}"),
    }
}

fn reject(at: &JsonPointer, message: String) -> RejectError {
    RejectError::new(RejectKind::UnsatisfiableConfig, message).at(at.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::config::{Config, Pagination};
    use crate::diag::Ctx;
    use crate::{contract, doc::parse as doc_parse, normalize, resolve, shape};

    /// A listing operation with a cursor, a next cursor and a page of items.
    fn document() -> Value {
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "parameters": [
                            {"name": "cursor", "in": "query", "schema": {"type": "string"}},
                        ],
                        "responses": {"200": {
                            "description": "ok",
                            "content": {"application/json": {
                                "schema": {"$ref": "#/components/schemas/Page"},
                            }},
                        }},
                    },
                },
            },
            "components": {"schemas": {
                "Page": {
                    "type": "object",
                    "required": ["items"],
                    "properties": {
                        "items": {"type": "array", "items": {"$ref": "#/components/schemas/Pet"}},
                        "next": {"type": "string"},
                        "total": {"type": "integer"},
                    },
                },
                "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            }},
        })
    }

    fn build(declared: Pagination) -> Result<(), String> {
        let config = Config {
            pagination: [("list_pets".to_owned(), declared)].into_iter().collect(),
            ..Config::default()
        };
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document(), &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx).unwrap();
        super::super::build(&resolved, &shapes, &contracts, &config, &mut ctx)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn declaring(cursor: &str, next: &str, items: &str) -> Pagination {
        Pagination {
            cursor_param: cursor.to_owned(),
            next_cursor: next.to_owned(),
            items: items.to_owned(),
        }
    }

    #[test]
    fn a_declaration_the_document_supports_is_accepted() {
        assert_eq!(build(declaring("cursor", "next", "items")), Ok(()));
    }

    /// Each refusal names what it looked for and what the document had instead.
    ///
    /// The message matters as much as the refusal: a caller who mistypes a member name is owed the
    /// list of members, not "invalid configuration". These are the four ways a declaration can be
    /// wrong, and the corpus survey is why they are worth spelling out — 62 documents paginate and
    /// every one of them will be declared by hand.
    #[test]
    fn a_cursor_parameter_the_operation_does_not_have_is_refused_by_name() {
        let error = build(declaring("page", "next", "items")).unwrap_err();
        assert!(error.contains("`page`"), "{error}");
        assert!(error.contains("`cursor`"), "{error}");
    }

    #[test]
    fn a_member_that_does_not_exist_is_refused_with_the_ones_that_do() {
        let error = build(declaring("cursor", "nextCursor", "items")).unwrap_err();
        assert!(error.contains("nextCursor"), "{error}");
        assert!(error.contains("`items`"), "{error}");
    }

    #[test]
    fn items_that_are_not_a_list_are_refused() {
        let error = build(declaring("cursor", "next", "total")).unwrap_err();
        assert!(error.contains("rather than a list"), "{error}");
    }

    /// A next cursor that is always present would never end the stream.
    #[test]
    fn a_required_next_cursor_is_refused() {
        let error = build(declaring("cursor", "items", "items")).unwrap_err();
        assert!(error.contains("always present"), "{error}");
    }

    #[test]
    fn a_declaration_for_an_operation_that_does_not_exist_is_refused() {
        let config = Config {
            pagination: [(
                "list_owners".to_owned(),
                declaring("cursor", "next", "items"),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document(), &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx).unwrap();
        let error = super::super::build(&resolved, &shapes, &contracts, &config, &mut ctx)
            .expect_err("an operation that does not exist cannot be declared");
        assert!(error.to_string().contains("list_owners"), "{error}");
    }
}
