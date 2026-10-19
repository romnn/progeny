//! A JSON Schema bundle read as the types it defines, for [`crate::generate_schemas`].
//!
//! A bundle has no operations: its `definitions` or `$defs` are the types, and the caller says
//! which of them it sends and receives.
//! Each namespace member is parsed at the pointer it sits at, so a `$ref` in either of its
//! spellings reaches it as written and a diagnostic points into the bundle, and it is named the
//! way a `components.schemas` entry is.
//! The roots are the wire: only the types they reach are generated, and their directions decide
//! what a collapse costs and which types get a read form.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};
use crate::doc::{Components, Direction, Document, ParsedDocument, Selected};
use crate::schema::{SchemaId, SchemaStore};

/// What [`crate::generate_schemas`] reads out of a bundle: where its types are, and which of
/// them the caller exchanges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaInput {
    /// JSON Pointers to the objects whose members are the bundle's named types, in precedence
    /// order.
    ///
    /// A name two namespaces define names the first one's type.
    /// A later definition that repeats that schema, apart from annotations such as `title` and
    /// `$schema`, is that type wherever it is referenced.
    /// One that differs is reported and stays reachable by its own pointer.
    /// A namespace nested in another, such as `/definitions/v2` in `/definitions`, is not a type
    /// of the outer one.
    /// Empty means the bundle's own `/$defs` and `/definitions`, in that order, where they exist.
    pub namespaces: Vec<String>,
    /// The types the caller sends or receives, by the names their namespaces give them.
    ///
    /// Only what these reach is generated, so there has to be at least one.
    pub roots: Vec<SchemaRoot>,
}

/// One type the caller exchanges, and which way it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaRoot {
    /// The name a namespace gives the type.
    pub name: String,
    /// Which way the type travels, which stands in for the request or response position an
    /// operation would give it.
    pub direction: Direction,
}

/// The namespaces a bundle holds its types in when the caller names none.
const DEFAULT_NAMESPACES: [&str; 2] = ["/$defs", "/definitions"];

/// Read a bundle into the model an OpenAPI document parses into, with its roots selected.
///
/// The root is read as the schema it is, so `#` and the root's `$id` name it and every
/// `definitions` and `$defs` member below it is registered where it sits.
/// A namespace member nothing registered yet is read on its own, and one already registered keeps
/// its one identity, so a namespace inside another names schemas it does not read twice.
///
/// # Errors
///
/// Returns [`RejectError`] when the bundle is not an object, when a namespace is not an object
/// in it, or when no root is selected or a root names no type, because generating anything else
/// would not be what the caller asked for.
pub(crate) fn read(
    value: Value,
    input: &SchemaInput,
    ctx: &mut Ctx,
) -> Result<ParsedDocument, RejectError> {
    let mut root = value;
    let Value::Object(members) = &root else {
        return Err(RejectError::new(
            RejectKind::NotAnObject,
            "a JSON Schema bundle is an object whose members hold its schemas",
        ));
    };
    let legacy = members
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(crate::normalize::declares_legacy_draft);
    let namespaces = namespaces(&root, input)?;
    // Kept as written, so a `$ref` to a position the rewrite moves still finds what it addresses.
    let written = root.clone();
    let mut moves = crate::normalize::bundle_schema_at(&mut root, legacy, JsonPointer::root(), ctx);
    let (store, registered) = register(&root, &written, &namespaces, legacy, &mut moves, ctx);
    let Named { named, repeats } = name(&written, &namespaces, &store, &registered, legacy, ctx);
    let roots = select(input, &named)?;

    // The whole bundle stays readable, so a `$ref` to a position nothing read is still read as
    // the schema it addresses.
    let Value::Object(members) = written else {
        return Err(RejectError::new(
            RejectKind::NotAnObject,
            "a JSON Schema bundle is an object whose members hold its schemas",
        ));
    };
    let document = Document {
        components: Some(Components {
            schemas: Some(named),
            ..Components::default()
        }),
        extensions: members.into_iter().collect(),
        ..Document::default()
    };
    Ok(ParsedDocument {
        document,
        schemas: store,
        dialect_30: false,
        roots,
        bundle_dialect: Some(legacy),
        moves,
        repeats,
    })
}

/// Parse the root and every namespace member, each at its pointer, and index them by it.
///
/// `root` is the bundle as normalized and `written` as the bundle wrote it: a namespace is read
/// from what the bundle wrote, because a draft-07 `$ref` beside it may have dropped it from the
/// normalized root, and each member is read by the draft of the resource it sits in.
fn register(
    root: &Value,
    written: &Value,
    namespaces: &[JsonPointer],
    legacy: bool,
    moves: &mut Vec<crate::normalize::Move>,
    ctx: &mut Ctx,
) -> (SchemaStore, BTreeMap<String, SchemaId>) {
    // The root and everything its own keywords hold, each at its pointer.
    let mut store = SchemaStore::default();
    let _ = crate::schema::parse::schema(root.clone(), &mut store, ctx);
    let mut registered: BTreeMap<String, SchemaId> = store
        .iter()
        .map(|(id, _)| (store.address(id).to_string(), id))
        .collect();

    // Every namespace member, outermost namespace first, so a member an outer one already read
    // is found rather than read again.
    let mut by_depth: Vec<&JsonPointer> = namespaces.iter().collect();
    by_depth.sort_by_key(|namespace| namespace.tokens().len());
    for namespace in by_depth {
        let Some(Value::Object(entries)) = lookup(written, namespace) else {
            continue;
        };
        for (name, member) in entries {
            let at = namespace.child(name.clone());
            // A namespace listed on its own is not a type of the one it sits in.
            if namespaces.contains(&at)
                || !(member.is_object() || member.is_boolean())
                || registered.contains_key(&at.to_string())
            {
                continue;
            }
            // A member the root's rewrite moved is the schema it moved, under both pointers.
            if let Some(&id) = crate::normalize::moved_address(&at, moves)
                .and_then(|rewritten| registered.get(&rewritten))
            {
                registered.insert(at.to_string(), id);
                continue;
            }
            let mut member = member.clone();
            let legacy = legacy_along(Some(written), at.tokens(), legacy);
            moves.extend(crate::normalize::bundle_schema_at(
                &mut member,
                legacy,
                at.clone(),
                ctx,
            ));
            let before = store.len();
            let _ = at_pointer(ctx, at.tokens(), |ctx| {
                crate::schema::parse::schema(member, &mut store, ctx)
            });
            registered.extend(
                store
                    .iter()
                    .skip(before)
                    .map(|(id, _)| (store.address(id).to_string(), id)),
            );
        }
    }
    (store, registered)
}

/// What [`name`] found: the type each name names, and the definitions that repeat it.
struct Named {
    named: BTreeMap<String, SchemaId>,
    /// Each schema of a later definition that repeats the first one, with the schema at the same
    /// place in the first.
    repeats: BTreeMap<SchemaId, SchemaId>,
}

/// The type each name names: the first namespace's, in the caller's order, to define it.
///
/// A later definition of the name that only repeats the first is that schema, and nothing is
/// reported; one that differs is.
fn name(
    root: &Value,
    namespaces: &[JsonPointer],
    store: &SchemaStore,
    registered: &BTreeMap<String, SchemaId>,
    legacy: bool,
    ctx: &mut Ctx,
) -> Named {
    let mut named: BTreeMap<String, SchemaId> = BTreeMap::new();
    let mut first: BTreeMap<String, JsonPointer> = BTreeMap::new();
    let mut repeats = BTreeMap::new();
    for namespace in namespaces {
        let Some(Value::Object(entries)) = lookup(root, namespace) else {
            continue;
        };
        for name in entries.keys() {
            let at = namespace.child(name.clone());
            let Some(&id) = registered
                .get(&at.to_string())
                .filter(|_| !namespaces.contains(&at))
            else {
                continue;
            };
            if let (Some(earlier), Some(&canonical)) = (first.get(name), named.get(name)) {
                if repeated(root, earlier, &at, legacy) {
                    repeats.extend(same_places(store, registered, id, canonical));
                    continue;
                }
                // The sentence names no pointer, so a bundle with a habit of shadowing is one
                // record.
                ctx.report(Diagnostic::new(
                    BreakageClass::CollidingTypeName,
                    Action::Warn,
                    at,
                    "a namespace listed earlier defines this name too, so its schema names the \
                     type; this one keeps its own pointer, and a reference to it still reaches it",
                ));
            } else {
                named.insert(name.clone(), id);
                first.insert(name.clone(), at);
            }
        }
    }
    // A repeat of a schema that is itself a repeat, found later, is the schema that one repeats,
    // and so is a name whose first definition sits inside a repeat.
    let repeats = settled(&repeats);
    for id in named.values_mut() {
        *id = repeats.get(id).copied().unwrap_or(*id);
    }
    Named { named, repeats }
}

/// Each repeat with the schema at the end of its chain of repeats.
fn settled(repeats: &BTreeMap<SchemaId, SchemaId>) -> BTreeMap<SchemaId, SchemaId> {
    repeats
        .iter()
        .map(|(&repeat, &next)| {
            let mut seen = BTreeSet::from([repeat]);
            let mut at = next;
            while let Some(&further) = repeats.get(&at) {
                if !seen.insert(at) {
                    break;
                }
                at = further;
            }
            (repeat, at)
        })
        .filter(|(repeat, at)| repeat != at)
        .collect()
}

/// Each schema the definition `repeated` holds, with the schema at the same place in `canonical`.
///
/// Only places a keyword holds schemas at are followed, because those are the places
/// [`repeated`] compared, and the two definitions are the same schema there.
/// A schema registered inside an annotation, or inside a keyword progeny does not read, keeps its
/// own identity.
fn same_places(
    store: &SchemaStore,
    registered: &BTreeMap<String, SchemaId>,
    repeated: SchemaId,
    canonical: SchemaId,
) -> BTreeMap<SchemaId, SchemaId> {
    let (from, to) = (store.address(repeated), store.address(canonical));
    store
        .iter()
        .filter_map(|(id, _)| {
            let rest = store
                .address(id)
                .tokens()
                .strip_prefix(from.tokens())
                .filter(|rest| schema_path(rest))?;
            let place = rest
                .iter()
                .fold(to.clone(), |at, token| at.child(token.clone()));
            let &same = registered.get(&place.to_string())?;
            (same != id).then_some((id, same))
        })
        .collect()
}

/// Whether `tokens` lead from a schema to a schema through keywords that hold schemas alone.
fn schema_path(mut tokens: &[String]) -> bool {
    while let Some((keyword, rest)) = tokens.split_first() {
        tokens = if SCHEMA_MAPS.contains(&keyword.as_str()) {
            let Some((_, rest)) = rest.split_first() else {
                return false;
            };
            rest
        } else if SUBSCHEMAS.contains(&keyword.as_str()) {
            // An array of schemas is entered by its index.
            match rest.split_first() {
                Some((index, rest)) if index.parse::<usize>().is_ok() => rest,
                _ => rest,
            }
        } else {
            return false;
        };
    }
    true
}

/// Annotations a schema can differ in and still be the schema another one is, because none of
/// them changes what it accepts.
const ANNOTATIONS: [&str; 4] = ["title", "description", "$comment", "examples"];

/// Keywords whose value is a schema or an array of schemas.
const SUBSCHEMAS: [&str; 16] = [
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "items",
    "prefixItems",
    "additionalItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedItems",
    "unevaluatedProperties",
    "contentSchema",
];

/// Keywords whose value maps names to schemas.
///
/// A draft-07 `dependencies` entry may be an array of names, which is compared as written.
const SCHEMA_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// Whether the definition at `then` repeats the one at `first`: the two are read by one draft and
/// are the same schema once [`ANNOTATIONS`] are set aside wherever a schema sits.
///
/// The definitions' own `$schema` is set aside once they are read by one draft, and their own
/// `$id` where neither holds a reference it would be the base of.
fn repeated(root: &Value, first: &JsonPointer, then: &JsonPointer, legacy: bool) -> bool {
    let (Some(left), Some(right)) = (lookup(root, first), lookup(root, then)) else {
        return false;
    };
    if legacy_along(Some(root), first.tokens(), legacy)
        != legacy_along(Some(root), then.tokens(), legacy)
    {
        return false;
    }
    let with_id = holds_reference(left) || holds_reference(right);
    let (Value::Object(left), Value::Object(right)) = (left, right) else {
        return left == right;
    };
    let (mut left, mut right) = (members(left), members(right));
    for own in [&mut left, &mut right] {
        own.retain(|&key, _| key != "$schema" && (with_id || key != "$id"));
    }
    same_members(left, right)
}

/// A schema's members by keyword.
fn members(schema: &serde_json::Map<String, Value>) -> BTreeMap<&str, &Value> {
    schema
        .iter()
        .map(|(key, member)| (key.as_str(), member))
        .collect()
}

/// Whether two schemas are the same once [`ANNOTATIONS`] are set aside.
fn same_schema(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => same_members(members(left), members(right)),
        _ => left == right,
    }
}

/// Whether two schemas' members are the same once [`ANNOTATIONS`] are set aside, each compared as
/// what its keyword holds.
fn same_members(mut left: BTreeMap<&str, &Value>, mut right: BTreeMap<&str, &Value>) -> bool {
    left.retain(|key, _| !ANNOTATIONS.contains(key));
    right.retain(|key, _| !ANNOTATIONS.contains(key));
    left.len() == right.len()
        && left.iter().all(|(&key, left)| {
            right
                .get(key)
                .is_some_and(|right| same_keyword(key, left, right))
        })
}

/// Whether two values of the keyword `key` are the same, as schemas where it holds schemas and as
/// written otherwise.
fn same_keyword(key: &str, left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) if SCHEMA_MAPS.contains(&key) => {
            left.len() == right.len()
                && left.iter().all(|(name, left)| {
                    right
                        .get(name)
                        .is_some_and(|right| same_schema(left, right))
                })
        }
        (Value::Array(left), Value::Array(right)) if SUBSCHEMAS.contains(&key) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_schema(left, right))
        }
        _ if SUBSCHEMAS.contains(&key) => same_schema(left, right),
        _ => left == right,
    }
}

/// Whether `value` holds a `$ref` or `$dynamicRef` anywhere, a member named so included.
fn holds_reference(value: &Value) -> bool {
    match value {
        Value::Object(members) => members.iter().any(|(key, member)| {
            matches!(key.as_str(), "$ref" | "$dynamicRef") || holds_reference(member)
        }),
        Value::Array(items) => items.iter().any(holds_reference),
        _ => false,
    }
}

/// The roots, as the schemas their names name.
fn select(
    input: &SchemaInput,
    named: &BTreeMap<String, SchemaId>,
) -> Result<Vec<Selected>, RejectError> {
    let roots = input
        .roots
        .iter()
        .map(|root| match named.get(&root.name) {
            Some(id) => Ok(Selected {
                id: *id,
                name: root.name.clone(),
                direction: root.direction,
            }),
            None => Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the root `{}` is not a name any namespace of the bundle defines",
                    root.name
                ),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if roots.is_empty() {
        return Err(RejectError::new(
            RejectKind::UnsatisfiableConfig,
            "no root is selected, and only what the roots reach is generated, so there is \
             nothing to generate",
        ));
    }
    Ok(roots)
}

/// Whether the schema at `tokens` in a bundle is read by draft-07 or an earlier draft.
///
/// The nearest `$schema` on the way down to it decides, and the bundle's own `legacy` where
/// none does.
pub(crate) fn legacy_at(document: &Document, tokens: &[String], legacy: bool) -> bool {
    let Some((first, rest)) = tokens.split_first() else {
        return legacy;
    };
    legacy_along(document.extensions.get(first), rest, legacy)
}

/// [`legacy_at`], from `node` down along `rest`.
fn legacy_along(mut node: Option<&Value>, mut rest: &[String], mut legacy: bool) -> bool {
    loop {
        if let Some(Value::Object(members)) = node
            && let Some(Value::String(uri)) = members.get("$schema")
        {
            legacy = crate::normalize::declares_legacy_draft(uri);
        }
        let Some((token, tail)) = rest.split_first() else {
            return legacy;
        };
        node = match node {
            Some(Value::Object(members)) => members.get(token),
            Some(Value::Array(items)) => token.parse::<usize>().ok().and_then(|at| items.get(at)),
            _ => None,
        };
        rest = tail;
    }
}

/// The namespaces to read, as pointers, in precedence order.
fn namespaces(root: &Value, input: &SchemaInput) -> Result<Vec<JsonPointer>, RejectError> {
    if input.namespaces.is_empty() {
        return Ok(DEFAULT_NAMESPACES
            .iter()
            .filter_map(|written| JsonPointer::parse(written))
            .filter(|pointer| matches!(lookup(root, pointer), Some(Value::Object(_))))
            .collect());
    }
    input
        .namespaces
        .iter()
        .map(|written| {
            let pointer = JsonPointer::parse(written).ok_or_else(|| {
                RejectError::new(
                    RejectKind::UnsatisfiableConfig,
                    format!("the namespace `{written}` is not a JSON Pointer"),
                )
            })?;
            match lookup(root, &pointer) {
                Some(Value::Object(_)) => Ok(pointer),
                _ => Err(RejectError::new(
                    RejectKind::UnsatisfiableConfig,
                    format!("the namespace `{written}` is not an object in the bundle"),
                )),
            }
        })
        .collect()
}

/// The value at `pointer` in the bundle, the root itself for the empty pointer.
fn lookup<'a>(root: &'a Value, pointer: &JsonPointer) -> Option<&'a Value> {
    pointer
        .tokens()
        .iter()
        .try_fold(root, |value, token| match value {
            Value::Object(members) => members.get(token),
            Value::Array(items) => items.get(token.parse::<usize>().ok()?),
            _ => None,
        })
}

/// Run `f` with the context scoped at `tokens`, so what it parses and reports sits there.
fn at_pointer<T>(ctx: &mut Ctx, tokens: &[String], f: impl FnOnce(&mut Ctx) -> T) -> T {
    match tokens.split_first() {
        None => f(ctx),
        Some((head, rest)) => ctx.scoped(head, |ctx| at_pointer(ctx, rest, f)),
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::json;

    use super::{Direction, SchemaInput, SchemaRoot};
    use crate::{BreakageClass, Config, Output, RejectKind, generate_schemas};

    /// A draft-07 bundle in the shape codex's app-server protocol has: a nested `v2` namespace
    /// that shadows one name of the outer one, references in both spellings, and a type no root
    /// reaches.
    fn bundle() -> Vec<u8> {
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Shared": {"type": "string", "enum": ["old"]},
                "Unused": {"type": "object", "properties": {"x": {"type": "string"}}},
                // draft-07 ignores the `type` beside the `$ref`.
                "Legacy": {"$ref": "#/definitions/v2/Thread/properties/id", "type": "integer"},
                "ThreadStartParams": {
                    "type": "object",
                    "required": ["thread"],
                    "properties": {
                        "thread": {"$ref": "#/definitions/v2/Thread"},
                        "note": {"type": ["string", "null"]},
                    },
                },
                "v2": {
                    "Shared": {"type": "string", "enum": ["new"]},
                    "Thread": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "shared": {"$ref": "#/definitions/v2/Shared"},
                            "legacy": {"$ref": "#/definitions/Legacy"},
                        },
                    },
                    "ThreadStartResponse": {
                        "type": "object",
                        "required": ["thread"],
                        "properties": {"thread": {"$ref": "#/definitions/v2/Thread"}},
                    },
                },
            },
        })
        .to_string()
        .into_bytes()
    }

    fn input(roots: &[(&str, Direction)]) -> SchemaInput {
        SchemaInput {
            namespaces: vec!["/definitions/v2".to_owned(), "/definitions".to_owned()],
            roots: roots
                .iter()
                .map(|(name, direction)| SchemaRoot {
                    name: (*name).to_owned(),
                    direction: *direction,
                })
                .collect(),
        }
    }

    fn types(output: &Output) -> String {
        output
            .files
            .iter()
            .find(|(path, _)| path.as_str().ends_with("types.rs"))
            .map(|(_, source)| source.clone())
            .unwrap_or_default()
    }

    /// The roots' closure is generated under the bundle's names, references in both spellings
    /// reach their schemas, and what no root reaches is left out.
    #[test_util::test]
    fn a_bundle_generates_what_its_roots_reach_under_its_own_names() {
        let output = generate_schemas(
            &bundle(),
            &input(&[
                ("ThreadStartParams", Direction::Sent),
                ("ThreadStartResponse", Direction::Received),
            ]),
            &Config::default(),
        )?;
        let source = types(&output);
        for name in [
            "pub struct ThreadStartParams",
            "pub struct ThreadStartResponse",
            "pub struct Thread ",
            "pub enum Shared",
        ] {
            assert!(source.contains(name), "{name}: {source}");
        }
        // The nested namespace is not a type of the outer one, and nothing reaches `Unused`.
        assert!(!source.contains("pub struct V2"), "{source}");
        assert!(!source.contains("Unused"), "{source}");
        // `v2` comes first, so its `Shared` names the type.
        assert!(source.contains("New"), "{source}");
        // The `type` beside `Legacy`'s `$ref` was dropped, so it is the string it references.
        assert!(source.contains("pub type Legacy = String;"), "{source}");
        // A received root gets a read form, and a sent-only one does not.
        assert!(source.contains("pub mod read"), "{source}");
        let read = source.split("pub mod read").nth(1).unwrap_or_default();
        assert!(read.contains("pub struct ThreadStartResponse"), "{read}");
        assert!(!read.contains("pub struct ThreadStartParams"), "{read}");
        let classes: Vec<(BreakageClass, String)> = output
            .diagnostics
            .iter()
            .map(|found| (found.class(), found.location().to_string()))
            .collect();
        assert!(
            classes.contains(&(
                BreakageClass::CollidingTypeName,
                "/definitions/Shared".to_owned()
            )),
            "{classes:?}"
        );
        assert!(
            classes.contains(&(
                BreakageClass::LegacyRefSiblings,
                "/definitions/Legacy".to_owned()
            )),
            "{classes:?}"
        );
        // draft-07 is read by its own rules, so it is no dialect read as 2020-12.
        assert!(
            !classes
                .iter()
                .any(|(class, _)| *class == BreakageClass::UnsupportedDialect),
            "{classes:?}"
        );
        // The bundle has no operations, and none is asked for.
        assert!(
            !classes
                .iter()
                .any(|(class, _)| *class == BreakageClass::PresenceCollapse
                    && output
                        .diagnostics
                        .iter()
                        .any(|found| found.detail().contains("no operation"))),
            "{classes:?}"
        );
    }

    /// A collapse is priced by the way its type travels, as the roots say.
    #[test_util::test]
    fn a_collapse_is_described_by_the_root_that_reaches_it() {
        let output = generate_schemas(
            &bundle(),
            &input(&[("ThreadStartParams", Direction::Sent)]),
            &Config::default(),
        )?;
        let collapse = output
            .diagnostics
            .iter()
            .find(|found| found.class() == BreakageClass::PresenceCollapse)
            .ok_or_else(|| eyre::eyre!("no collapse in {:?}", output.diagnostics))?;
        assert!(
            collapse.detail().contains("a root the caller sends"),
            "{}",
            collapse.detail()
        );
        assert!(
            collapse.detail().contains("cannot send an explicit null"),
            "{}",
            collapse.detail()
        );
    }

    /// Generate `schema`'s types for the root `Root`, received.
    fn generated(schema: &serde_json::Value, input: &SchemaInput) -> eyre::Result<Output> {
        Ok(generate_schemas(
            schema.to_string().as_bytes(),
            input,
            &Config::default(),
        )?)
    }

    fn received(namespaces: &[&str]) -> SchemaInput {
        SchemaInput {
            namespaces: namespaces
                .iter()
                .map(|&namespace| namespace.to_owned())
                .collect(),
            roots: vec![SchemaRoot {
                name: "Root".to_owned(),
                direction: Direction::Received,
            }],
        }
    }

    /// The bundle root is a schema with an identity: `#` and the root's `$id` both name it, and a
    /// pointer behind the `$id` reaches what the plain pointer does.
    #[test_util::test]
    fn the_bundle_root_is_a_schema_references_reach() {
        let bundle = json!({
            "$id": "https://example.test/bundle.json",
            "$defs": {
                "Root": {"type": "object", "properties": {
                    "node": {"$ref": "https://example.test/bundle.json#/$defs/Node"},
                }},
                "Node": {"type": "object", "properties": {
                    "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}},
                    "label": {"type": "string"},
                }},
            },
        });
        let output = generated(&bundle, &received(&[]))?;
        let source = types(&output);
        assert!(source.contains("pub node: Option<Node>"), "{source}");
        assert!(
            source.contains("pub children: Option<Vec<Node>>"),
            "{source}"
        );
        assert!(
            !output
                .diagnostics
                .iter()
                .any(|found| found.class() == BreakageClass::DanglingRef),
            "{:?}",
            output.diagnostics
        );
    }

    /// The root object can be a namespace, as a bundle whose members are its types writes it.
    #[test_util::test]
    fn the_root_object_can_be_the_namespace() {
        let bundle = json!({"Root": {"type": "object", "properties": {"a": {"type": "string"}}}});
        let source = types(&generated(&bundle, &received(&[""]))?);
        assert!(source.contains("pub struct Root"), "{source}");
    }

    /// A namespace inside another names schemas the outer one already read, so an anchor in them
    /// is declared once, whichever namespace is listed first.
    #[test_util::test]
    fn a_namespace_inside_another_reads_its_schemas_once() {
        let bundle = json!({
            "$defs": {
                "Root": {"type": "object", "properties": {"item": {"$ref": "#item"}}},
                "Container": {"$defs": {
                    "Item": {"$anchor": "item", "type": "object",
                             "properties": {"x": {"type": "string"}}},
                }},
            },
        });
        for namespaces in [
            ["/$defs", "/$defs/Container/$defs"],
            ["/$defs/Container/$defs", "/$defs"],
        ] {
            let output = generated(&bundle, &received(&namespaces))?;
            let source = types(&output);
            assert!(
                source.contains("pub item: Option<Item>"),
                "{namespaces:?}: {source}"
            );
        }
    }

    /// A discriminator mapping by bare name names a bundle's own type, wherever it sits.
    #[test_util::test]
    fn a_bare_mapping_name_names_a_bundle_type() {
        let subtype = |own: &str| {
            json!({"allOf": [{"$ref": "#/definitions/Root"}, {
                "type": "object", "properties": {own: {"type": "string"}},
            }]})
        };
        let bundle = json!({"definitions": {
            "Root": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"type": "string"}},
                "discriminator": {"propertyName": "kind", "mapping": {"cat": "Cat", "dog": "Dog"}},
            },
            "Cat": subtype("meow"),
            "Dog": subtype("bark"),
        }});
        let source = types(&generated(&bundle, &received(&[]))?);
        assert!(source.contains("pub enum Root"), "{source}");
        assert!(
            source.contains("Cat(") && source.contains("Dog("),
            "{source}"
        );
    }

    /// A draft-07 bundle's rules reach a schema only a `$ref` proves is one, and a schema that
    /// names its own draft is read by it.
    #[test_util::test]
    fn each_schema_is_read_by_the_draft_it_is_written_in() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "helper": {"$ref": "#/helpers/helper"},
                    "modern": {"$ref": "#/definitions/Modern"},
                }},
                "Base": {"type": "object", "properties": {"a": {"type": "string"}}},
                "Modern": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "$ref": "#/definitions/Base",
                    "required": ["a"],
                },
            },
            // Not a namespace: only the `$ref` says this is a schema.
            "helpers": {"helper": {"$ref": "#/definitions/Base", "required": ["a"]}},
        });
        let output = generated(&bundle, &received(&[]))?;
        let dropped: Vec<String> = output
            .diagnostics
            .iter()
            .filter(|found| found.class() == BreakageClass::LegacyRefSiblings)
            .flat_map(|found| {
                std::iter::once(found.location().to_string())
                    .chain(found.related().iter().map(ToString::to_string))
            })
            .collect();
        // The helper is draft-07 like its bundle, and `Modern` is 2020-12 by its own `$schema`.
        assert!(
            dropped.contains(&"/helpers/helper".to_owned()),
            "{dropped:?}"
        );
        assert!(
            !dropped.contains(&"/definitions/Modern".to_owned()),
            "{dropped:?}"
        );
    }

    /// A position the draft-07 rewrite moves is still what a `$ref` to it reaches, and a schema
    /// only a `$ref` reaches is read by the draft of the resource it sits in.
    #[test_util::test]
    fn a_reference_reaches_what_the_bundle_wrote() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "moved": {"$ref": "#/definitions/Host/dependencies/x"},
                    "named": {"$ref": "urn:dep"},
                    "nested": {"$ref": "#/definitions/Modern/helpers/h"},
                }},
                "Base": {"type": "object", "properties": {"a": {"type": "string"}}},
                "Host": {"type": "object", "dependencies": {
                    "x": {"$id": "urn:dep", "type": "object",
                          "properties": {"y": {"type": "integer"}}},
                }},
                "Modern": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "helpers": {"h": {"$ref": "#/definitions/Base", "required": ["a"]}},
                },
            },
        });
        let output = generated(&bundle, &received(&[]))?;
        let source = types(&output);
        assert!(source.contains("pub y: Option<i64>"), "{source}");
        // The moved schema is one identity, so its `$id` is declared once and both references
        // reach it.
        assert!(
            !output.diagnostics.iter().any(|found| {
                found.class() == BreakageClass::DanglingRef
                    && found.action() == crate::Action::Degrade
            }),
            "{:?}",
            output.diagnostics
        );
        // `h` sits in a 2020-12 resource, so its `required` beside the `$ref` stands.
        assert!(
            !output.diagnostics.iter().any(|found| {
                found.class() == BreakageClass::LegacyRefSiblings
                    && std::iter::once(found.location())
                        .chain(found.related())
                        .any(|at| at.to_string() == "/definitions/Modern/helpers/h")
            }),
            "{:?}",
            output.diagnostics
        );
    }

    /// A reference reaches a moved schema only where normalization really moved it: an `items`
    /// that is no tuple keeps `additionalItems` where it is, and a schema writing both spellings
    /// of its dependencies keeps both.
    #[test_util::test]
    fn a_reference_reaches_a_moved_schema_only_where_it_moved() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "extra": {"$ref": "#/definitions/List/additionalItems"},
                    "old": {"$ref": "#/definitions/Both/dependencies/x"},
                }},
                "List": {
                    "type": "array",
                    "items": {"type": "string"},
                    "additionalItems": {"type": "integer"},
                },
                "Both": {
                    "type": "object",
                    "dependencies": {"x": {"type": "boolean"}},
                    "dependentSchemas": {"x": {"type": "number"}},
                },
            },
        });
        let source = types(&generated(&bundle, &received(&[]))?);
        assert!(source.contains("pub extra: Option<i64>"), "{source}");
        assert!(source.contains("pub old: Option<bool>"), "{source}");
    }

    /// A tuple's tail and a dependency inside a schema only a `$ref` reaches are each one
    /// identity, reached by pointer and by `$id` alike.
    #[test_util::test]
    fn a_moved_schema_is_one_identity_wherever_it_was_read() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "tail": {"$ref": "#/definitions/Pair/additionalItems"},
                    "tail_by_id": {"$ref": "urn:tail"},
                    "host": {"$ref": "#/helpers/Host"},
                    "dep": {"$ref": "#/helpers/Host/dependencies/x"},
                    "dep_by_id": {"$ref": "urn:dep"},
                }},
                "Pair": {
                    "type": "array",
                    "items": [{"type": "string"}],
                    "additionalItems": {"$id": "urn:tail", "type": "integer"},
                },
            },
            "helpers": {"Host": {"type": "object", "dependencies": {
                "x": {"$id": "urn:dep", "type": "boolean"},
            }}},
        });
        let output = generated(&bundle, &received(&[]))?;
        let source = types(&output);
        assert!(source.contains("pub tail: Option<i64>"), "{source}");
        assert!(source.contains("pub tail_by_id: Option<i64>"), "{source}");
        assert!(source.contains("pub dep: Option<bool>"), "{source}");
        assert!(source.contains("pub dep_by_id: Option<bool>"), "{source}");
        assert!(
            !output.diagnostics.iter().any(|found| {
                found.class() == BreakageClass::DanglingRef
                    && found.action() == crate::Action::Degrade
            }),
            "{:?}",
            output.diagnostics
        );
    }

    /// A namespace inside a position the draft-07 rewrite moves names the schemas the root read
    /// there, so a moved schema keeps one identity.
    #[test_util::test]
    fn a_namespace_inside_a_moved_position_reads_nothing_twice() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "dep": {"$ref": "urn:dep"},
                    "named": {"$ref": "#/definitions/Host/dependencies/Dep"},
                }},
                "Host": {"type": "object", "dependencies": {
                    "Dep": {"$id": "urn:dep", "type": "object",
                            "properties": {"y": {"type": "integer"}}},
                }},
            },
        });
        let output = generated(
            &bundle,
            &received(&["/definitions", "/definitions/Host/dependencies"]),
        )?;
        let source = types(&output);
        assert!(source.contains("pub struct Dep"), "{source}");
        assert!(source.contains("pub dep: Option<Dep>"), "{source}");
        assert!(source.contains("pub named: Option<Dep>"), "{source}");
        assert!(
            !output.diagnostics.iter().any(|found| {
                found.class() == BreakageClass::DanglingRef
                    && found.action() == crate::Action::Degrade
            }),
            "{:?}",
            output.diagnostics
        );
    }

    /// A namespace a draft-07 `$ref` beside it would drop is still read, and its members by the
    /// draft of the resource they sit in.
    #[test_util::test]
    fn a_namespace_is_read_as_the_bundle_wrote_it() {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$ref": "#/schemas/Root",
            "schemas": {
                "Root": {"type": "object", "properties": {"a": {"type": "string"}}},
            },
        });
        let source = types(&generated(&bundle, &received(&["/schemas"]))?);
        assert!(source.contains("pub struct Root"), "{source}");

        let bundle = json!({
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "h": {"$ref": "#/definitions/Container/helpers/H"},
                }},
                "Base": {"type": "object", "properties": {"a": {"type": "string"}}},
                "Container": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "helpers": {"H": {"$ref": "#/definitions/Base", "required": ["a"]}},
                },
            },
        });
        let output = generated(
            &bundle,
            &received(&["/definitions", "/definitions/Container/helpers"]),
        )?;
        // `H` sits in a draft-07 resource, so its `required` beside the `$ref` is dropped.
        assert!(
            output.diagnostics.iter().any(|found| {
                found.class() == BreakageClass::LegacyRefSiblings
                    && std::iter::once(found.location())
                        .chain(found.related())
                        .any(|at| at.to_string() == "/definitions/Container/helpers/H")
            }),
            "{:?}",
            output.diagnostics
        );
    }

    /// The default namespaces are the bundle's own `$defs` and `definitions`, a root exchanged
    /// both ways is a request and a response, and strict decoding makes no read form.
    #[test_util::test]
    fn defaults_both_directions_and_strict_decoding() {
        let bundle = json!({"definitions": {"Root": {
            "type": "object",
            "required": ["id"],
            "properties": {"id": {"type": "string"}, "note": {"type": ["string", "null"]}},
        }}});
        let mut input = received(&[]);
        input.roots[0].direction = Direction::Both;
        let output = generated(&bundle, &input)?;
        let collapse = output
            .diagnostics
            .iter()
            .find(|found| found.class() == BreakageClass::PresenceCollapse)
            .ok_or_else(|| eyre::eyre!("no collapse in {:?}", output.diagnostics))?;
        assert!(
            collapse
                .detail()
                .contains("a root the caller sends and one it receives"),
            "{}",
            collapse.detail()
        );
        assert!(types(&output).contains("pub mod read"));
        let strict = Config {
            decoding: crate::config::Decoding::Strict,
            ..Config::default()
        };
        let output = generate_schemas(bundle.to_string().as_bytes(), &input, &strict)?;
        assert!(
            !types(&output).contains("pub mod read"),
            "{}",
            types(&output)
        );
    }

    /// The `Id` both namespaces of [`repeating`] define, as the `v2` one does.
    fn id() -> serde_json::Value {
        json!({"anyOf": [{"type": "string"}, {"type": "integer", "format": "int64"}]})
    }

    /// A bundle whose `v2` namespace defines `Id` as `first` and whose outer one defines it again
    /// as `then`, read by a root that reaches both, with what it generated and the pointers of its
    /// collisions.
    fn repeating(
        first: &serde_json::Value,
        then: &serde_json::Value,
    ) -> eyre::Result<(String, Vec<String>)> {
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "outer": {"$ref": "#/definitions/Id"},
                    "inner": {"$ref": "#/definitions/v2/Id"},
                }},
                "Id": then,
                "Text": {"type": "string"},
                "v2": {"Id": first},
            },
        });
        let output = generated(&bundle, &received(&["/definitions/v2", "/definitions"]))?;
        let collisions = output
            .diagnostics
            .iter()
            .filter(|found| found.class() == BreakageClass::CollidingTypeName)
            .map(|found| found.location().to_string())
            .collect();
        Ok((types(&output), collisions))
    }

    /// A name two namespaces define as one schema, annotations aside, is one type, and nothing
    /// is lost to report.
    #[test_util::test]
    fn a_definition_repeated_in_another_namespace_is_one_type() {
        let annotated = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "Identifier",
            "description": "The id.",
            "$comment": "Exported twice.",
            "examples": ["a"],
            "$id": "https://example.test/id.json",
            "anyOf": [{"type": "string"}, {"type": "integer", "format": "int64"}],
        });
        for then in [id(), annotated] {
            let (source, collisions) = repeating(&id(), &then)?;
            assert!(collisions.is_empty(), "{then}: {collisions:?}");
            assert!(source.contains("pub outer: Option<Id>"), "{then}: {source}");
            assert!(source.contains("pub inner: Option<Id>"), "{then}: {source}");
            assert!(!source.contains("Identifier"), "{then}: {source}");
        }
    }

    /// Annotations are set aside wherever a schema sits, and a member's own name or a listed
    /// value is compared as written.
    #[test_util::test]
    fn a_repeat_may_differ_in_annotations_below_its_root() {
        let object = |description: &str| {
            json!({"type": "object", "properties": {
                "value": {"type": "string", "description": description, "title": description},
            }})
        };
        let (source, collisions) = repeating(&object("Old."), &object("New."))?;
        assert!(collisions.is_empty(), "{collisions:?}");
        assert!(source.contains("pub outer: Option<Id>"), "{source}");
        assert!(source.contains("pub inner: Option<Id>"), "{source}");

        // A member called `title` and a listed `description` are what the schema accepts.
        let members =
            |name: &str| json!({"type": "object", "properties": {name: {"type": "string"}}});
        let (_, collisions) = repeating(&members("title"), &members("description"))?;
        assert_eq!(collisions, ["/definitions/Id"]);
        let listed = |value: &str| json!({"enum": [{"description": value}]});
        let (_, collisions) = repeating(&listed("a"), &listed("b"))?;
        assert_eq!(collisions, ["/definitions/Id"]);
    }

    /// A repeat is the schema it repeats by every route to it: its `$id`, an anchor both declare,
    /// and its pointer where draft-07 normalization moved the first definition.
    #[test_util::test]
    fn a_repeat_is_the_schema_it_repeats_by_every_route() {
        let id = id();
        let mut titled = id.clone();
        titled["title"] = json!("Identifier");
        titled["$id"] = json!("urn:id");
        let mut anchored = id.clone();
        anchored["$anchor"] = json!("id");
        let mut anchored_titled = anchored.clone();
        anchored_titled["title"] = json!("Identifier");
        let bundles = [
            // By `$id`
            json!({"definitions": {
                "Root": {"type": "object", "properties": {
                    "outer": {"$ref": "urn:id"},
                    "inner": {"$ref": "#/definitions/v2/Id"},
                }},
                "Id": titled,
                "v2": {"Id": id},
            }}),
            // By an anchor both declare
            json!({"$defs": {
                "Root": {"type": "object", "properties": {
                    "outer": {"$ref": "#id"},
                    "inner": {"$ref": "#/$defs/Id"},
                }},
                "Id": anchored_titled,
                "v2": {"Id": anchored},
            }}),
        ];
        for (bundle, namespaces) in bundles
            .iter()
            .zip([["/definitions/v2", "/definitions"], ["/$defs/v2", "/$defs"]])
        {
            let output = generated(bundle, &received(&namespaces))?;
            let source = types(&output);
            assert!(source.contains("pub outer: Option<Id>"), "{source}");
            assert!(source.contains("pub inner: Option<Id>"), "{source}");
            assert!(!source.contains("Identifier"), "{source}");
            assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        }

        // The first `Id` is a draft-07 dependency, which normalization moves to
        // `dependentSchemas`, and nothing references the pointer it was written at.
        let mut first = id.clone();
        first["$id"] = json!("urn:first");
        let mut then = id.clone();
        then["title"] = json!("Identifier");
        let bundle = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Root": {"type": "object", "properties": {
                    "outer": {"$ref": "#/definitions/Id"},
                    "inner": {"$ref": "urn:first"},
                }},
                "Host": {"type": "object", "dependencies": {"Id": first}},
                "Id": then,
            },
        });
        let output = generated(
            &bundle,
            &received(&["/definitions/Host/dependencies", "/definitions"]),
        )?;
        let source = types(&output);
        assert!(source.contains("pub outer: Option<Id>"), "{source}");
        assert!(source.contains("pub inner: Option<Id>"), "{source}");
        assert!(!source.contains("Identifier"), "{source}");
    }

    /// A repeat is the end of a chain of repeats, a name whose first definition sits inside a
    /// repeat names the schema that one repeats, and a schema read inside an annotation keeps its
    /// own identity.
    #[test_util::test]
    fn nested_namespaces_reach_one_schema_per_repeat() {
        // `third`'s `Foo` repeats `second`'s, which repeats `first`'s, so the anchor all three
        // declare names one schema.
        let foo = json!({"type": "string", "$anchor": "foo"});
        let boxed = json!({"type": "object", "$defs": {"Foo": foo}});
        let bundle = json!({
            "first": {
                "Foo": foo,
                "Root": {"type": "object", "properties": {"foo": {"$ref": "#foo"}}},
            },
            "second": {"Box": boxed},
            "third": {"Box": boxed},
        });
        let output = generated(
            &bundle,
            &received(&["/first", "/second", "/second/Box/$defs", "/third"]),
        )?;
        let source = types(&output);
        assert!(source.contains("pub foo: Option<Foo>"), "{source}");
        assert!(source.contains("pub type Foo = String;"), "{source}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);

        // The root `Foo` is first defined inside `b`'s `Box`, which repeats `a`'s.
        let titled = |title: &str| {
            json!({"type": "object", "$defs": {"Foo": {
                "type": "object", "title": title, "properties": {"v": {"type": "string"}},
            }}})
        };
        let bundle = json!({
            "a": {
                "Box": titled("CanonicalFoo"),
                "Root": {"type": "object", "properties": {"foo": {"$ref": "#/b/Box/$defs/Foo"}}},
            },
            "b": {"Box": titled("OtherFoo")},
        });
        let mut input = received(&["/a", "/b", "/b/Box/$defs"]);
        input.roots.push(SchemaRoot {
            name: "Foo".to_owned(),
            direction: Direction::Received,
        });
        let source = types(&generated(&bundle, &input)?);
        assert!(source.contains("pub foo: Option<Foo>"), "{source}");
        // The titles are only the schema's documentation.
        assert!(!source.contains("struct CanonicalFoo"), "{source}");
        assert!(!source.contains("struct OtherFoo"), "{source}");

        // `examples` are set aside, but a schema a namespace reads out of one is not the one at
        // the same place in the other example.
        let example = |kind: &str| json!({"type": "object", "examples": [{"X": {"type": kind}}]});
        let bundle = json!({
            "a": {
                "Id": example("string"),
                "Root": {"type": "object", "properties": {"x": {"$ref": "#/b/Id/examples/0/X"}}},
            },
            "b": {"Id": example("integer")},
        });
        let output = generated(
            &bundle,
            &received(&["/a", "/b", "/a/Id/examples/0", "/b/Id/examples/0"]),
        )?;
        let source = types(&output);
        assert!(source.contains("pub x: Option<i64>"), "{source}");
    }

    /// A repeated name whose schemas differ in what they accept is still reported, and so is one
    /// read by another draft or one whose `$id` is the base of a reference.
    #[test_util::test]
    fn a_name_defined_differently_still_collides() {
        let referencing = json!({
            "anyOf": [{"$ref": "#/definitions/Text"}, {"type": "integer", "format": "int64"}],
        });
        let mut identified = referencing.clone();
        identified["$id"] = json!("https://example.test/id.json");
        let cases = [
            (
                id(),
                json!({"anyOf": [{"type": "string"}, {"type": "number", "format": "int64"}]}),
            ),
            (
                id(),
                json!({"anyOf": [{"type": "string"}, {"type": "integer", "format": "int32"}]}),
            ),
            (id(), json!({"anyOf": [{"type": "string"}]})),
            (
                id(),
                json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "anyOf": [{"type": "string"}, {"type": "integer", "format": "int64"}],
                }),
            ),
            (referencing, identified),
            (
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
                json!({"type": "object", "properties": {
                    "a": {"type": "string"},
                    "b": {"type": "string"},
                }}),
            ),
        ];
        for (first, then) in cases {
            let (_, collisions) = repeating(&first, &then)?;
            assert_eq!(collisions, ["/definitions/Id"], "{then}");
        }
    }

    /// A root that names no type, or a namespace the bundle does not hold, is refused rather
    /// than generating something else.
    #[test_util::test]
    fn an_unknown_root_or_namespace_is_refused() {
        let unknown = generate_schemas(
            &bundle(),
            &input(&[("Missing", Direction::Sent)]),
            &Config::default(),
        );
        assert!(
            matches!(&unknown, Err(error) if error.kind() == RejectKind::UnsatisfiableConfig),
            "{unknown:?}"
        );
        let mut absent = input(&[("ThreadStartParams", Direction::Sent)]);
        absent.namespaces.push("/$defs".to_owned());
        let absent = generate_schemas(&bundle(), &absent, &Config::default());
        assert!(
            matches!(&absent, Err(error) if error.kind() == RejectKind::UnsatisfiableConfig),
            "{absent:?}"
        );
        let none = generate_schemas(&bundle(), &input(&[]), &Config::default());
        assert!(none.is_err(), "{none:?}");
    }
}
