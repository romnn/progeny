//! A JSON Schema bundle read as the types it defines, for [`crate::generate_schemas`].
//!
//! A bundle has no operations: its `definitions` or `$defs` are the types, and the caller says
//! which of them it sends and receives.
//! Each namespace member is parsed at the pointer it sits at, so a `$ref` in either of its
//! spellings reaches it as written and a diagnostic points into the bundle, and it is named the
//! way a `components.schemas` entry is.
//! The roots are the wire: only the types they reach are generated, and their directions decide
//! what a collapse costs and which types get a read form.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};
use crate::doc::{Components, Document, ParsedDocument};
use crate::schema::{SchemaId, SchemaStore};

/// What [`crate::generate_schemas`] reads out of a bundle: where its types are, and which of
/// them the caller exchanges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaInput {
    /// JSON Pointers to the objects whose members are the bundle's named types, in precedence
    /// order.
    ///
    /// A name two namespaces define names the first one's type, and the other is reported and
    /// stays reachable by its own pointer.
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

/// Which way a type travels between the caller and its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The caller builds and sends it, as it would a request.
    Sent,
    /// The caller receives and reads it, as it would a response.
    Received,
    /// Both.
    Both,
}

impl Direction {
    pub(crate) fn sent(self) -> bool {
        matches!(self, Self::Sent | Self::Both)
    }

    pub(crate) fn received(self) -> bool {
        matches!(self, Self::Received | Self::Both)
    }
}

/// A root, as the schema that defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selected {
    pub(crate) id: SchemaId,
    pub(crate) name: String,
    pub(crate) direction: Direction,
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
    let named = name(&written, &namespaces, &registered, ctx);
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

/// The type each name names: the first namespace's, in the caller's order, to define it.
fn name(
    root: &Value,
    namespaces: &[JsonPointer],
    registered: &BTreeMap<String, SchemaId>,
    ctx: &mut Ctx,
) -> BTreeMap<String, SchemaId> {
    let mut named: BTreeMap<String, SchemaId> = BTreeMap::new();
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
            // The sentence names no pointer, so a bundle with a habit of shadowing is one record.
            if named.contains_key(name) {
                ctx.report(Diagnostic::new(
                    BreakageClass::CollidingTypeName,
                    Action::Warn,
                    at,
                    "a namespace listed earlier defines this name too, so its schema names the \
                     type; this one keeps its own pointer, and a reference to it still reaches it",
                ));
            } else {
                named.insert(name.clone(), id);
            }
        }
    }
    named
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
                    && output.diagnostics.iter().any(|found| found
                        .detail()
                        .contains("no operation"))),
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
            namespaces: namespaces.iter().map(|&namespace| namespace.to_owned()).collect(),
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
        assert!(source.contains("pub children: Option<Vec<Node>>"), "{source}");
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
            assert!(source.contains("pub item: Option<Item>"), "{namespaces:?}: {source}");
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
        assert!(source.contains("Cat(") && source.contains("Dog("), "{source}");
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
        assert!(dropped.contains(&"/helpers/helper".to_owned()), "{dropped:?}");
        assert!(!dropped.contains(&"/definitions/Modern".to_owned()), "{dropped:?}");
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
        assert!(!types(&output).contains("pub mod read"), "{}", types(&output));
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
