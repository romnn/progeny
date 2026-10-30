//! How a generated server reads a value that arrived as text: the plan for each position.
//!
//! A URL, a form body and a multipart body carry no types.
//! Whether a piece of text is the value itself or the spelling of a number, a boolean or a
//! compound is a fact about the type at that position *and* about how the client wrote it, so the
//! plan is decided here, from both, and shipped to the server's `support::Reading` as data.
//!
//! The plan is exact at the leaves: a position typed as a string keeps its text whatever its
//! siblings need.
//! It follows the type's structure only as deep as the wire does.
//! A parameter, and each member of a form or multipart body, arrives with its own compound level
//! rebuilt from the wire — an array split or repeated, an object rebuilt from `deepObject` or a
//! flat `form` spelling — and every compound below that level was written as one JSON text, which
//! is read once as [`Reading::Spelled`].
//!
//! Aliases, newtypes and flattened unions add no level, so the plan follows them by name, and a
//! name it meets again on the same path is a cycle, which reads as [`Reading::Spelled`].

use crate::api::{PartKind, PartSpec};
use crate::contract::{
    Capture, ContractKind, Contracts, FieldContract, TypeContract, TypeIndex, TypeRef,
};

/// The compound levels a parameter rebuilds from the wire: its own.
const PARAMETER_LEVELS: u8 = 1;

/// The compound levels a form or multipart body rebuilds from the wire: the body's own object,
/// and each declared member's own level.
const BODY_LEVELS: u8 = 2;

/// How the generated server reads one position's text back into the value the description types.
///
/// The model of `support::Reading`, which the server renderer writes out as tokens position by
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reading {
    /// The value as it arrived: a string leaf, a multipart file or JSON part, a JSON-content
    /// parameter.
    Keep,
    /// The JSON value the text spells, where the type cannot say whether the text is a string, or
    /// where the client wrote a whole compound as one JSON text.
    Spelled,
    /// An array whose elements each read by the row.
    Items(Box<Reading>),
    /// An array read position by position.
    Tuple(Vec<Reading>),
    /// An object whose members each read by the row that names them, and by `others` where no row
    /// does.
    Object {
        members: Vec<MemberReading>,
        others: Box<Reading>,
    },
    /// A compound that arrives either as one JSON text of the whole value, which is parsed once and
    /// read no further, or already rebuilt, which reads by the row.
    Whole(Box<Reading>),
}

/// One named member's row in a [`Reading::Object`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberReading {
    /// The member's exact name on the wire.
    pub(crate) wire_name: String,
    pub(crate) reading: Reading,
}

/// The plan for a parameter of type `ty` that the style rows read from text.
///
/// A parameter declared with a JSON `content` is not one of these: its text is parsed as JSON
/// before the plan runs, so it reads [`Reading::Keep`].
pub(crate) fn parameter(ty: &TypeRef, contracts: &Contracts) -> Reading {
    Plan { contracts }.of(ty, PARAMETER_LEVELS, &[])
}

/// The plan for an `application/x-www-form-urlencoded` body of type `ty`.
///
/// A captured member has no row in the form table, and the client writes it as the query rule's
/// default — one raw occurrence per element of a list, the JSON text of an object — so it reads by
/// its value type at a member's level, exactly as a declared member does.
pub(crate) fn form(ty: &TypeRef, contracts: &Contracts) -> Reading {
    Plan { contracts }.body(ty, Arrival::Rebuilt(BODY_LEVELS - 1))
}

/// The plan for a `multipart/form-data` body of type `ty`, whose declared members' parts are
/// `parts`.
///
/// A file part arrives as the object an `Upload` reads and a JSON part as the value its JSON
/// spells, so both keep what the parser built.
/// A captured member has no row in the part table, so it reads as one JSON part of the whole value
/// or as repeated parts of a sequence, whichever arrived.
pub(crate) fn multipart(ty: &TypeRef, parts: &[PartSpec], contracts: &Contracts) -> Reading {
    let mut reading = Plan { contracts }.body(ty, Arrival::Whole);
    if let Reading::Object { members, .. } = &mut reading {
        for member in members {
            let built = parts.iter().any(|part| {
                part.wire_name == member.wire_name
                    && matches!(part.kind, PartKind::File | PartKind::Json)
            });
            if built {
                member.reading = Reading::Keep;
            }
        }
    }
    reading
}

/// How a member that no row describes reaches the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arrival {
    /// Rebuilt from the wire like a declared member, with this many compound levels left to read
    /// structurally.
    Rebuilt(u8),
    /// As a multipart member without a row.
    ///
    /// The client writes a compound one as a single JSON part of the whole value, and a foreign
    /// client may send a sequence as repeated parts instead, which the parser rebuilds into an
    /// array of part texts.
    /// The member reads as [`Reading::Whole`] over the plan of one rebuilt level, so either
    /// spelling reads back, and a scalar keeps its exact leaf reading.
    Whole,
}

/// The rows an object position reads its members by, before it becomes a [`Reading::Object`].
#[derive(Debug, Default)]
struct Rows {
    members: Vec<MemberReading>,
    /// The reading of a member no row names, when something captures such members.
    others: Option<Reading>,
}

impl Rows {
    /// The rows of a branch the plan reached again on its own path, which can read anything.
    fn cyclic() -> Self {
        Self {
            members: Vec::new(),
            others: Some(Reading::Spelled),
        }
    }

    fn named(&self, wire_name: &str) -> Option<&Reading> {
        self.members
            .iter()
            .find(|member| member.wire_name == wire_name)
            .map(|member| &member.reading)
    }

    fn into_reading(self) -> Reading {
        Reading::Object {
            members: self.members,
            others: Box::new(self.others.unwrap_or(Reading::Spelled)),
        }
    }
}

struct Plan<'a> {
    contracts: &'a Contracts,
}

impl Plan<'_> {
    fn kind(&self, index: TypeIndex) -> Option<&ContractKind> {
        self.contracts.get(index).map(TypeContract::kind)
    }

    /// A body's plan: its own object's rows, with captured members read by `captured`.
    fn body(&self, ty: &TypeRef, captured: Arrival) -> Reading {
        let mut ty = ty;
        let mut path: Vec<TypeIndex> = Vec::new();
        loop {
            match strip(ty) {
                TypeRef::Named(index) if !path.contains(index) => match self.kind(*index) {
                    Some(ContractKind::Struct { fields }) => {
                        path.push(*index);
                        return self
                            .struct_rows(fields, Arrival::Rebuilt(BODY_LEVELS - 1), captured, &path)
                            .into_reading();
                    }
                    Some(
                        ContractKind::Alias { target: inner } | ContractKind::Newtype { inner },
                    ) => {
                        path.push(*index);
                        ty = inner;
                    }
                    _ => break,
                },
                TypeRef::Map(element) => {
                    return Reading::Object {
                        members: Vec::new(),
                        others: Box::new(self.arrived(element, captured)),
                    };
                }
                _ => break,
            }
        }
        self.of(ty, BODY_LEVELS, &[])
    }

    /// The plan for a member that reaches the reader by `arrival`.
    fn arrived(&self, ty: &TypeRef, arrival: Arrival) -> Reading {
        match arrival {
            Arrival::Rebuilt(levels) => self.of(ty, levels, &[]),
            Arrival::Whole => match self.of(ty, 1, &[]) {
                compound @ (Reading::Items(_) | Reading::Tuple(_) | Reading::Object { .. }) => {
                    Reading::Whole(Box::new(compound))
                }
                leaf => leaf,
            },
        }
    }

    /// The plan for a value of type `ty` at a position with `levels` compound levels left to read
    /// structurally, having followed the names on `path` to get here without consuming one.
    fn of(&self, ty: &TypeRef, levels: u8, path: &[TypeIndex]) -> Reading {
        match ty {
            // Transparent: these say nothing about what the wire carries
            TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Boxed(inner) => {
                self.of(inner, levels, path)
            }
            // Leaves, exact at any depth
            TypeRef::String | TypeRef::Format(_) | TypeRef::Upload => Reading::Keep,
            TypeRef::Unit | TypeRef::Bool | TypeRef::Integer(_) | TypeRef::F64 | TypeRef::Value => {
                Reading::Spelled
            }
            // Compounds, each consuming one level
            TypeRef::Vec(element) | TypeRef::Array(element, _) => compound(levels, |inner| {
                Reading::Items(Box::new(self.of(element, inner, &[])))
            }),
            TypeRef::Tuple(items) => self.tuple(items, levels),
            TypeRef::Map(element) => compound(levels, |inner| Reading::Object {
                members: Vec::new(),
                others: Box::new(self.of(element, inner, &[])),
            }),
            TypeRef::Named(index) => self.named(*index, levels, path),
        }
    }

    fn named(&self, index: TypeIndex, levels: u8, path: &[TypeIndex]) -> Reading {
        // A name met again before any level was consumed only ever names itself.
        if path.contains(&index) {
            return Reading::Spelled;
        }
        let Some(kind) = self.kind(index) else {
            return Reading::Spelled;
        };
        match kind {
            ContractKind::StringEnum { .. } => Reading::Keep,
            ContractKind::Alias { target: inner } | ContractKind::Newtype { inner } => {
                self.of(inner, levels, &extended(path, index))
            }
            ContractKind::Tuple { items } => self.tuple(items, levels),
            ContractKind::Struct { fields } => compound(levels, |inner| {
                self.struct_rows(
                    fields,
                    Arrival::Rebuilt(inner),
                    Arrival::Rebuilt(inner),
                    &[index],
                )
                .into_reading()
            }),
            // A union's text could be any of its branches, so it is the ambiguous leaf.
            ContractKind::Enum { .. }
            | ContractKind::TaggedEnum { .. }
            | ContractKind::CarriedTagEnum { .. }
            | ContractKind::KeyedEnum { .. } => Reading::Spelled,
        }
    }

    fn tuple(&self, items: &[TypeRef], levels: u8) -> Reading {
        compound(levels, |inner| {
            Reading::Tuple(items.iter().map(|item| self.of(item, inner, &[])).collect())
        })
    }

    /// A struct's rows: its declared members read by `declared`, and what its capture holds by
    /// `captured`.
    ///
    /// `path` holds the struct and the names that led to it through flattened unions, which add no
    /// level and so are where a cycle can hide.
    fn struct_rows(
        &self,
        fields: &[FieldContract],
        declared: Arrival,
        captured: Arrival,
        path: &[TypeIndex],
    ) -> Rows {
        let mut rows = Rows {
            members: fields
                .iter()
                .filter(|field| !field.is_capture())
                .map(|field| MemberReading {
                    wire_name: field.wire_name.clone(),
                    reading: self.arrived(&field.ty, declared),
                })
                .collect(),
            others: None,
        };
        let Some(capture) = fields.iter().find(|field| field.is_capture()) else {
            return rows;
        };
        match capture.capture {
            // A map of what the description declared, or of arbitrary JSON where it declared
            // nothing: either way, the map's value type.
            Some(Capture::Declared | Capture::Undeclared) | None => {
                rows.others = Some(match strip(&capture.ty) {
                    TypeRef::Map(element) => self.arrived(element, captured),
                    _ => Reading::Spelled,
                });
            }
            Some(Capture::Union) => {
                let union = self.union_rows(&capture.ty, captured, path);
                for member in union.members {
                    if rows.named(&member.wire_name).is_none() {
                        rows.members.push(member);
                    }
                }
                rows.others = union.others;
            }
        }
        rows
    }

    /// The rows a union flattened beside a struct's members reads the remaining members by.
    ///
    /// Each branch reads a member by its own row for it, or else by its own capture.
    /// A member gets a row only where every branch that reads it at all reads it the same way,
    /// because which branch the payload is cannot be known before it is read.
    /// The others are the branches' captures where those agree, and [`Reading::Spelled`] where
    /// they do not.
    /// A union that dispatches on a tag member matches the tag's text, so that member keeps it
    /// whatever its branches declare.
    fn union_rows(&self, ty: &TypeRef, arrival: Arrival, path: &[TypeIndex]) -> Rows {
        let TypeRef::Named(index) = strip(ty) else {
            return Rows::default();
        };
        if path.contains(index) {
            return Rows::cyclic();
        }
        let path = extended(path, *index);
        let (branches, tag): (Vec<&TypeRef>, Option<&str>) = match self.kind(*index) {
            Some(ContractKind::Enum { variants, .. }) => {
                (variants.iter().map(|variant| &variant.ty).collect(), None)
            }
            Some(
                ContractKind::TaggedEnum { tag, variants, .. }
                | ContractKind::CarriedTagEnum { tag, variants, .. },
            ) => (
                variants.iter().map(|variant| &variant.ty).collect(),
                Some(tag.as_str()),
            ),
            Some(ContractKind::Alias { target: inner } | ContractKind::Newtype { inner }) => {
                return self.union_rows(inner, arrival, &path);
            }
            _ => return Rows::default(),
        };
        let branches: Vec<Rows> = branches
            .into_iter()
            .filter_map(|branch| self.branch_rows(branch, arrival, &path))
            .collect();

        let mut names: Vec<&str> = Vec::new();
        for branch in &branches {
            for member in &branch.members {
                if !names.contains(&member.wire_name.as_str()) {
                    names.push(&member.wire_name);
                }
            }
        }
        let mut members: Vec<MemberReading> = names
            .into_iter()
            .filter(|name| Some(*name) != tag)
            .filter_map(|name| {
                let reading = agreed(
                    branches
                        .iter()
                        .filter_map(|branch| branch.named(name).or(branch.others.as_ref())),
                )?;
                Some(MemberReading {
                    wire_name: name.to_owned(),
                    reading,
                })
            })
            .collect();
        if let Some(tag) = tag {
            members.push(MemberReading {
                wire_name: tag.to_owned(),
                reading: Reading::Keep,
            });
        }
        let captures: Vec<&Reading> = branches
            .iter()
            .filter_map(|branch| branch.others.as_ref())
            .collect();
        let others = if captures.is_empty() {
            None
        } else {
            Some(agreed(captures).unwrap_or(Reading::Spelled))
        };
        Rows { members, others }
    }

    /// The rows one branch of a flattened union reads an object by, when it reads objects at all.
    fn branch_rows(&self, ty: &TypeRef, arrival: Arrival, path: &[TypeIndex]) -> Option<Rows> {
        match strip(ty) {
            TypeRef::Map(element) => Some(Rows {
                members: Vec::new(),
                others: Some(self.arrived(element, arrival)),
            }),
            TypeRef::Value => Some(Rows {
                members: Vec::new(),
                others: Some(Reading::Spelled),
            }),
            TypeRef::Named(index) => {
                if path.contains(index) {
                    return Some(Rows::cyclic());
                }
                match self.kind(*index)? {
                    ContractKind::Struct { fields } => {
                        Some(self.struct_rows(fields, arrival, arrival, &extended(path, *index)))
                    }
                    ContractKind::Alias { target: inner } | ContractKind::Newtype { inner } => {
                        self.branch_rows(inner, arrival, &extended(path, *index))
                    }
                    ContractKind::Enum { .. }
                    | ContractKind::TaggedEnum { .. }
                    | ContractKind::CarriedTagEnum { .. } => {
                        Some(self.union_rows(strip(ty), arrival, path))
                    }
                    ContractKind::StringEnum { .. }
                    | ContractKind::KeyedEnum { .. }
                    | ContractKind::Tuple { .. } => None,
                }
            }
            // A branch that is not an object never reads the members.
            _ => None,
        }
    }
}

/// `path` with one more name on it.
fn extended(path: &[TypeIndex], index: TypeIndex) -> Vec<TypeIndex> {
    let mut extended = path.to_vec();
    extended.push(index);
    extended
}

/// A compound's plan when a level is left for it, and [`Reading::Spelled`] once the client wrote
/// it as one JSON text.
fn compound(levels: u8, build: impl FnOnce(u8) -> Reading) -> Reading {
    match levels.checked_sub(1) {
        Some(inner) => build(inner),
        None => Reading::Spelled,
    }
}

/// The one reading every candidate agrees on, if there are candidates and they all agree.
fn agreed<'a>(candidates: impl IntoIterator<Item = &'a Reading>) -> Option<Reading> {
    let mut candidates = candidates.into_iter();
    let first = candidates.next()?;
    candidates
        .all(|candidate| candidate == first)
        .then(|| first.clone())
}

/// A type without the wrappers that say nothing about the wire.
fn strip(ty: &TypeRef) -> &TypeRef {
    match ty {
        TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Boxed(inner) => strip(inner),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{MemberReading, Reading};
    use crate::api::BodyContract;
    use crate::api::tests::{model_of, with_paths};

    fn row(wire_name: &str, reading: Reading) -> MemberReading {
        MemberReading {
            wire_name: wire_name.to_owned(),
            reading,
        }
    }

    fn object(members: Vec<MemberReading>, others: Reading) -> Reading {
        Reading::Object {
            members,
            others: Box::new(others),
        }
    }

    /// The plan of the one body an operation over `schema` posts as `media_type`.
    fn body_reading(media_type: &str, schema: &Value) -> eyre::Result<Reading> {
        body_reading_in(media_type, schema, &json!({}))
    }

    /// The same, in a document whose component schemas are `schemas`.
    fn body_reading_in(media_type: &str, schema: &Value, schemas: &Value) -> eyre::Result<Reading> {
        let mut document = with_paths(json!({
            "/submit": {
                "post": {
                    "operationId": "submit",
                    "requestBody": {"required": true, "content": {media_type: {"schema": schema}}},
                    "responses": {"201": {"description": "made"}},
                },
            },
        }));
        document["components"] = json!({ "schemas": schemas });
        let (model, _) = model_of(document)?;
        let operation = model
            .operations()
            .first()
            .ok_or_eyre("the document declares one operation")?;
        match &operation.body {
            Some(BodyContract::Form { reading, .. } | BodyContract::Multipart { reading, .. }) => {
                Ok(reading.clone())
            }
            other => eyre::bail!("the body is not form or multipart: {other:?}"),
        }
    }

    /// The plan of the one query parameter `p` of an operation.
    fn parameter_reading(parameter: &Value) -> eyre::Result<Reading> {
        parameter_reading_in(parameter, &json!({}))
    }

    /// The same, in a document whose component schemas are `schemas`.
    fn parameter_reading_in(parameter: &Value, schemas: &Value) -> eyre::Result<Reading> {
        let mut document = with_paths(json!({
            "/search": {
                "get": {
                    "operationId": "search",
                    "parameters": [parameter.clone()],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        }));
        document["components"] = json!({ "schemas": schemas });
        let (model, _) = model_of(document)?;
        let operation = model
            .operations()
            .first()
            .ok_or_eyre("the document declares one operation")?;
        let param = operation
            .params
            .first()
            .ok_or_eyre("the operation declares one parameter")?;
        Ok(param.reading.clone())
    }

    /// A member captured by `additionalProperties` reads by the capture's value type, in a form
    /// body and in a `deepObject` parameter alike (#119).
    #[test_util::test]
    fn a_captured_member_reads_by_its_value_type() {
        let strings = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "additionalProperties": {"type": "string"},
        });
        let expected = object(vec![row("count", Reading::Spelled)], Reading::Keep);
        assert_eq!(
            body_reading("application/x-www-form-urlencoded", &strings)?,
            expected
        );
        assert_eq!(
            parameter_reading(&json!({
                "name": "p", "in": "query", "style": "deepObject", "explode": true,
                "schema": strings,
            }))?,
            expected
        );

        // A captured integer is still re-read as the number it spells.
        let integers = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "additionalProperties": {"type": "integer"},
        });
        assert_eq!(
            body_reading("application/x-www-form-urlencoded", &integers)?,
            object(vec![row("count", Reading::Spelled)], Reading::Spelled)
        );
    }

    /// An object member of a form body reads its own members exactly, which is what lets a flat
    /// `form` spelling keep a string member's text (#120).
    #[test_util::test]
    fn an_object_member_of_a_form_body_reads_its_own_members() {
        let schema = json!({
            "type": "object",
            "properties": {"payload": {
                "type": "object",
                "properties": {"label": {"type": "string"}, "count": {"type": "integer"}},
            }},
        });
        assert_eq!(
            body_reading("application/x-www-form-urlencoded", &schema)?,
            object(
                vec![row(
                    "payload",
                    object(
                        vec![row("count", Reading::Spelled), row("label", Reading::Keep)],
                        Reading::Spelled,
                    ),
                )],
                Reading::Spelled,
            )
        );
    }

    /// A captured list reads as raw occurrences in a form body, where the client writes one pair
    /// per element.
    /// In a multipart body the client writes it as one JSON part and a foreign client may repeat
    /// the part instead, so it reads as a whole JSON text or else element by element.
    #[test_util::test]
    fn a_captured_list_reads_by_how_the_client_wrote_it() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "additionalProperties": {"type": "array", "items": {"type": "string"}},
        });
        assert_eq!(
            body_reading("application/x-www-form-urlencoded", &schema)?,
            object(
                vec![row("name", Reading::Keep)],
                Reading::Items(Box::new(Reading::Keep)),
            )
        );
        assert_eq!(
            body_reading("multipart/form-data", &schema)?,
            object(
                vec![row("name", Reading::Keep)],
                Reading::Whole(Box::new(Reading::Items(Box::new(Reading::Keep)))),
            )
        );

        // A captured scalar keeps its exact leaf reading in a multipart body.
        let scalars = json!({
            "type": "object",
            "additionalProperties": {"type": "string"},
        });
        assert_eq!(
            body_reading("multipart/form-data", &scalars)?,
            object(Vec::new(), Reading::Keep)
        );
    }

    /// A parameter declared with a JSON `content` arrives parsed, so the plan keeps it whatever
    /// its type: re-reading the document's string `"1"` would change it.
    #[test_util::test]
    fn a_json_content_parameter_is_kept() {
        let reading = parameter_reading(&json!({
            "name": "p", "in": "query",
            "content": {"application/json": {"schema": {
                "type": "object",
                "properties": {"payload": {
                    "type": "object",
                    "properties": {"labels": {"type": "array", "items": {}}},
                }},
            }}},
        }))?;
        assert_eq!(reading, Reading::Keep);
    }

    /// A tuple parameter reads position by position, and only its own level is structural: an
    /// element that is itself a compound was written as one JSON text.
    #[test_util::test]
    fn a_tuple_parameter_reads_position_by_position() {
        let reading = parameter_reading(&json!({
            "name": "p", "in": "query",
            "schema": {
                "type": "array",
                "prefixItems": [
                    {"type": "string"},
                    {"type": "integer"},
                    {"type": "array", "items": {"type": "string"}},
                ],
                "items": false,
            },
        }))?;
        assert_eq!(
            reading,
            Reading::Tuple(vec![Reading::Keep, Reading::Spelled, Reading::Spelled])
        );
    }

    /// A union flattened beside a struct's members gives a member a row only where every branch
    /// reads it the same way: here one branch declares `label` a string and the other captures it
    /// as an integer, so neither reading can be chosen before the payload is.
    #[test_util::test]
    fn a_flattened_union_rows_only_what_its_branches_agree_on() {
        let schema = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "oneOf": [
                {"type": "object", "required": ["label", "note"],
                 "properties": {"label": {"type": "string"}, "note": {"type": "string"}}},
                {"type": "object", "required": ["note"],
                 "properties": {"note": {"type": "string"}},
                 "additionalProperties": {"type": "integer"}},
            ],
        });
        let Reading::Object { members, others } =
            body_reading("application/x-www-form-urlencoded", &schema)?
        else {
            eyre::bail!("the body reads as an object");
        };
        let named: Vec<(&str, &Reading)> = members
            .iter()
            .map(|member| (member.wire_name.as_str(), &member.reading))
            .collect();
        // Both branches declare `note` a string; only the first declares `label`.
        assert_eq!(
            named,
            [("count", &Reading::Spelled), ("note", &Reading::Keep)]
        );
        // The second branch alone captures, so the others read as its integers.
        assert_eq!(*others, Reading::Spelled);
    }

    /// A chain of names that each stand for the one before resolves exactly however long it is,
    /// so a string at its end keeps its text.
    #[test_util::test]
    fn a_long_chain_of_names_resolves_to_its_end() {
        let mut schemas = serde_json::Map::new();
        schemas.insert("T0".to_owned(), json!({"type": "string"}));
        for link in 1..=40 {
            schemas.insert(
                format!("T{link}"),
                json!({"oneOf": [{"$ref": format!("#/components/schemas/T{}", link - 1)}]}),
            );
        }
        let reading = parameter_reading_in(
            &json!({"name": "p", "in": "query", "schema": {"$ref": "#/components/schemas/T40"}}),
            &Value::Object(schemas),
        )?;
        assert_eq!(reading, Reading::Keep);
    }

    /// A type that contains itself is a real cycle, and the plan ends it where the wire does: each
    /// level the type contains itself at is one compound level, so a body member reads its own
    /// members exactly and the copy below that is one JSON text.
    /// The struct being on the plan's path does not cut its own member off early either.
    #[test_util::test]
    fn a_type_that_contains_itself_ends_where_the_wire_does() {
        let schemas = json!({
            "Node": {"type": "object", "properties": {
                "label": {"type": "string"},
                "next": {"$ref": "#/components/schemas/Node"},
            }},
        });
        let schema = json!({"type": "object", "properties": {
            "node": {"$ref": "#/components/schemas/Node"},
        }});
        assert_eq!(
            body_reading_in("application/x-www-form-urlencoded", &schema, &schemas)?,
            object(
                vec![row(
                    "node",
                    object(
                        vec![row("label", Reading::Keep), row("next", Reading::Spelled)],
                        Reading::Spelled,
                    ),
                )],
                Reading::Spelled,
            )
        );
    }

    /// A flattened union that dispatches on a tag member keeps the tag's text, even where its
    /// branches declare the member untyped and would otherwise agree on re-reading it: the tag
    /// `9` is matched as the string the mapping names.
    ///
    /// `Holder` uses `A` outside the union, which keeps the tag on the variant types, so the
    /// branches really do declare `kind`.
    #[test_util::test]
    fn a_dispatching_tag_keeps_its_text() {
        let schemas = json!({
            "A": {"type": "object", "required": ["kind"],
                  "properties": {"kind": {}, "a": {"type": "string"}}},
            "B": {"type": "object", "required": ["kind"],
                  "properties": {"kind": {}, "b": {"type": "string"}}},
            "Holder": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/A"}}},
        });
        let schema = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "oneOf": [
                {"$ref": "#/components/schemas/A"},
                {"$ref": "#/components/schemas/B"},
            ],
            "discriminator": {
                "propertyName": "kind",
        "mapping": {"9": "#/components/schemas/A", "b": "#/components/schemas/B"},
            },
        });
        let Reading::Object { members, .. } =
            body_reading_in("application/x-www-form-urlencoded", &schema, &schemas)?
        else {
            eyre::bail!("the body reads as an object");
        };
        let kind = members
            .iter()
            .find(|member| member.wire_name == "kind")
            .ok_or_eyre("the tag has a row")?;
        assert_eq!(kind.reading, Reading::Keep);
    }
}
