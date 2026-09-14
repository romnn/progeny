//! Which form each type takes, and the read twins of the types that need two.
//!
//! A response is decoded leniently: every member may be absent, `null`, or unreadable,
//! and the type it decodes into has to be able to say so.
//! A request is built by the caller, and the description's required-ness is enforced by
//! the type.
//! Where the two shapes are the same — no required member anywhere inside — one type
//! serves both directions.
//! Where they differ, a type reached from both directions gets a *read twin*: the same
//! contract with every required member made optional and a capture map for the members
//! the description does not declare.
//! A type reached from responses alone is simply written in that form; a type reached
//! from requests alone, or from nothing, keeps the strict one.
//!
//! The capture map alone does not earn a twin.
//! A shared type is one type, and a capture map on it would be written into requests too,
//! which the strict encoding must not do; so a shared type reports the undeclared members
//! it read and does not keep them, and only a type twinned for its required members keeps
//! them.
//! Twinning every both-direction struct for the map alone would roughly double the type
//! layer for a property whose detectable half — the report — a shared type already has.
//!
//! The difference propagates upward: a type holding a twinned type differs too, however
//! deep the required member sits, because the read form of the holder has to name the
//! read form of what it holds.
//! That is a fixed point over the type graph, computed here before anything is frozen.

use std::collections::{BTreeMap, BTreeSet};

use super::lower::Provisional;
use super::reach::Reach;
use super::{
    Capture, ContractKind, FieldContract, Form, Namer, Presence, RustIdent, SkipRule, TypeIndex,
    TypeRef,
};
use crate::shape::Docs;

/// Everything twinning decided: the twins it added, and the strict index each stands for.
#[derive(Debug, Default)]
pub(super) struct Twinned {
    /// Each twinned strict type's read twin.
    pub(super) twins: BTreeMap<TypeIndex, TypeIndex>,
}

/// Decide every type's form, appending a read twin where a type needs one.
///
/// `reach` is indexed like `types`.
/// New twins are pushed onto the end of `types`, so every index the caller already holds
/// stays valid.
pub(super) fn run(types: &mut Vec<Provisional>, reach: &[Reach]) -> Twinned {
    let differs = differs(types);
    let twinned: Vec<TypeIndex> = (0..types.len())
        .filter(|&index| {
            let at = reach.get(index).copied().unwrap_or_default();
            at.request && at.response && differs.contains(&index)
        })
        .map(|index| TypeIndex(u32::try_from(index).unwrap_or(u32::MAX)))
        .collect();
    let mut twins = BTreeMap::new();
    for (offset, &strict) in twinned.iter().enumerate() {
        let twin = TypeIndex(u32::try_from(types.len() + offset).unwrap_or(u32::MAX));
        twins.insert(strict, twin);
    }

    // Forms first, so the twins cloned below carry the strict half's final record.
    for (index, contract) in types.iter_mut().enumerate() {
        let at = reach.get(index).copied().unwrap_or_default();
        let here = TypeIndex(u32::try_from(index).unwrap_or(u32::MAX));
        contract.form = match (at.request, at.response) {
            (true, true) => match twins.get(&here) {
                Some(&twin) => Form::Strict { twin: Some(twin) },
                None => Form::Shared,
            },
            (false, true) => Form::Lenient { strict: None },
            (true | false, false) => Form::Strict { twin: None },
        };
    }
    for &strict in &twinned {
        let Some(original) = types.get(strict.index()) else {
            continue;
        };
        let mut twin = original.clone();
        twin.form = Form::Lenient {
            strict: Some(strict),
        };
        types.push(twin);
    }
    // Every read form — a response-only type rewritten in place, or a fresh twin — names the read
    // forms of what it holds and takes the lenient shape.
    for contract in types.iter_mut() {
        if !matches!(contract.form, Form::Lenient { .. }) {
            continue;
        }
        for ty in super::dedup::references_mut(&mut contract.kind) {
            ty.remap(&twins);
        }
        loosen(contract);
    }
    Twinned { twins }
}

/// The types whose read form has a different shape from their strict form.
///
/// A struct with a required member differs on its own; anything holding a differing type
/// differs through it.
/// Monotone, so the loop settles.
fn differs(types: &[Provisional]) -> BTreeSet<usize> {
    let mut differing: BTreeSet<usize> = types
        .iter()
        .enumerate()
        .filter(|(_, contract)| match &contract.kind {
            ContractKind::Struct { fields } => fields
                .iter()
                .any(|field| field.presence == Presence::Required && !field.is_capture()),
            ContractKind::Enum { .. }
            | ContractKind::TaggedEnum { .. }
            | ContractKind::CarriedTagEnum { .. }
            | ContractKind::StringEnum { .. }
            | ContractKind::Newtype { .. }
            | ContractKind::Tuple { .. }
            | ContractKind::Alias { .. } => false,
        })
        .map(|(index, _)| index)
        .collect();
    loop {
        let mut changed = false;
        for (index, contract) in types.iter().enumerate() {
            if differing.contains(&index) {
                continue;
            }
            let mut reached = Vec::new();
            for ty in contract.kind.references() {
                ty.named(&mut reached);
            }
            if reached.iter().any(|to| differing.contains(&to.index())) {
                differing.insert(index);
                changed = true;
            }
        }
        if !changed {
            return differing;
        }
    }
}

/// Put one contract into its read form.
///
/// A struct's required members become `Option`s left out of the wire when `None` — a member that
/// was absent or unreadable is written back absent, never as an invented `null` — and a capture
/// map joins the members unless the schema's own `additionalProperties` already put one there.
/// Every other kind keeps its shape: its read form differs only in what it names.
fn loosen(contract: &mut Provisional) {
    let ContractKind::Struct { fields } = &mut contract.kind else {
        return;
    };
    let mut used = Namer::default();
    for field in fields.iter_mut() {
        used.take(&field.rust_name);
        if field.presence == Presence::Required && !field.is_capture() {
            field.ty = TypeRef::Option(Box::new(std::mem::replace(&mut field.ty, TypeRef::Unit)));
            field.skip_serializing_if = SkipRule::WhenNone;
        }
    }
    if !fields.iter().any(FieldContract::is_capture) {
        fields.push(capture_member(&mut used));
    }
}

/// The member that holds what the description did not declare, as arbitrary JSON.
pub(super) fn capture_member(used: &mut Namer) -> FieldContract {
    FieldContract {
        rust_name: used.unique(RustIdent::field("extra")),
        // A flattened member has no wire name of its own: it *is* the leftovers.
        wire_name: String::new(),
        ty: TypeRef::Map(Box::new(TypeRef::Value)),
        presence: Presence::Required,
        default: None,
        skip_serializing_if: SkipRule::Never,
        capture: Some(Capture::Undeclared),
        docs: Docs {
            title: None,
            description: Some(
                "Members the description did not declare, kept verbatim and written back."
                    .to_owned(),
            ),
            deprecated: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use serde_json::{Value, json};

    use super::super::tests::{contracts_of, index_of, named};
    use super::super::{ContractKind, Contracts, Form, Presence, TypeRef};
    use crate::config::{Config, Decoding};

    /// A document with one operation sending `request` and answering with `response`, both by
    /// reference, over the given component schemas.
    fn document(request: &str, response: &str, schemas: &Value) -> Value {
        json!({
            "openapi": "3.1.0",
            "paths": {"/things": {"post": {
                "operationId": "postThing",
                "requestBody": {"content": {"application/json": {
                    "schema": {"$ref": format!("#/components/schemas/{request}")},
                }}},
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"$ref": format!("#/components/schemas/{response}")},
                }}}},
            }}},
            "components": {"schemas": schemas},
        })
    }

    fn schemas() -> Value {
        json!({
            "Sent": {"type": "object", "required": ["id"],
                     "properties": {"id": {"type": "string"},
                                    "tag": {"$ref": "#/components/schemas/Tag"}}},
            "Got": {"type": "object", "required": ["id"],
                    "properties": {"id": {"type": "string"},
                                   "tag": {"$ref": "#/components/schemas/Tag"},
                                   "both": {"$ref": "#/components/schemas/Both"}}},
            "Both": {"type": "object", "required": ["name"],
                     "properties": {"name": {"type": "string"},
                                    "note": {"$ref": "#/components/schemas/Note"}}},
            "Note": {"type": "object", "properties": {"text": {"type": "string"}}},
            "Tag": {"type": "string", "enum": ["a", "b"]},
            "Unreached": {"type": "object", "required": ["x"],
                          "properties": {"x": {"type": "integer"}}},
        })
    }

    /// The same schemas with `Sent` holding a `Both` too, so `Both` is reached from both
    /// directions and twins.
    fn schemas_sending_both() -> Value {
        let mut all = schemas();
        all["Sent"] = json!({
            "type": "object", "required": ["id"],
            "properties": {"id": {"type": "string"},
                           "both": {"$ref": "#/components/schemas/Both"},
                           "tag": {"$ref": "#/components/schemas/Tag"}},
        });
        all
    }

    fn form_of(contracts: &Contracts, name: &str) -> eyre::Result<Form> {
        Ok(named(contracts, name)?.form())
    }

    /// Each reach gets the form the design assigns it: request-only strict, response-only
    /// lenient, shared where nothing differs, and a twin where both directions meet a required
    /// member.
    #[test_util::test]
    fn forms_follow_reach_and_shape() {
        let (contracts, _) = contracts_of(
            document("Sent", "Got", &schemas_sending_both()),
            &Config::default(),
        )?;
        assert_eq!(form_of(&contracts, "Sent")?, Form::Strict { twin: None });
        assert_eq!(
            form_of(&contracts, "Unreached")?,
            Form::Strict { twin: None }
        );
        assert_eq!(form_of(&contracts, "Got")?, Form::Lenient { strict: None });
        // Reached from both, shape differs: a strict half with a twin at the end of the list.
        let both_index = index_of(&contracts, "Both")?;
        let Form::Strict { twin: Some(twin) } = form_of(&contracts, "Both")? else {
            eyre::bail!(
                "`Both` should have a twin: {:?}",
                form_of(&contracts, "Both")?
            );
        };
        let twin_contract = contracts
            .get(twin)
            .ok_or_else(|| eyre::eyre!("twin missing"))?;
        assert_eq!(twin_contract.rust_name().as_str(), "Both");
        assert_eq!(
            twin_contract.form(),
            Form::Lenient {
                strict: Some(both_index)
            }
        );
        // Reached from both, nothing required anywhere inside: one type serves both — and, being
        // what a request sends too, keeps no capture map for what a response adds.
        assert_eq!(form_of(&contracts, "Note")?, Form::Shared);
        let ContractKind::Struct { fields } = named(&contracts, "Note")?.kind() else {
            eyre::bail!("a struct");
        };
        assert!(!fields.iter().any(super::FieldContract::is_capture));
        assert_eq!(form_of(&contracts, "Tag")?, Form::Shared);
        assert!(contracts.has_read_forms());
        // The read form of a reference goes through the twin and leaves the rest alone.
        assert_eq!(
            contracts.read_form(&TypeRef::Vec(Box::new(TypeRef::Named(both_index)))),
            TypeRef::Vec(Box::new(TypeRef::Named(twin)))
        );
        let note = index_of(&contracts, "Note")?;
        assert_eq!(
            contracts.read_form(&TypeRef::Named(note)),
            TypeRef::Named(note)
        );
    }

    /// A read form makes every required member optional, skipped when `None`, and gains a
    /// capture map; it names the read forms of what it holds.
    #[test_util::test]
    fn a_read_form_is_loosened_and_captures() {
        let (contracts, _) = contracts_of(
            document("Sent", "Got", &schemas_sending_both()),
            &Config::default(),
        )?;
        let got = named(&contracts, "Got")?;
        let ContractKind::Struct { fields } = got.kind() else {
            eyre::bail!("a struct");
        };
        let id = fields
            .iter()
            .find(|field| field.wire_name == "id")
            .ok_or_else(|| eyre::eyre!("id"))?;
        assert_eq!(id.presence, Presence::Required, "the declaration is kept");
        assert!(matches!(id.ty, TypeRef::Option(_)));
        assert_eq!(id.skip_serializing_if, super::SkipRule::WhenNone);
        let extra = fields
            .iter()
            .find(|field| field.is_capture())
            .ok_or_else(|| eyre::eyre!("a capture member"))?;
        assert_eq!(extra.rust_name.as_str(), "extra");
        assert_eq!(extra.capture, Some(super::Capture::Undeclared));
        // `both` names the twin, not the strict half.
        let both = fields
            .iter()
            .find(|field| field.wire_name == "both")
            .ok_or_else(|| eyre::eyre!("both"))?;
        let TypeRef::Option(inner) = &both.ty else {
            eyre::bail!("optional");
        };
        let TypeRef::Named(index) = **inner else {
            eyre::bail!("named");
        };
        assert!(contracts.get(index).is_some_and(|c| c.form().is_lenient()));
        // The strict half is untouched.
        let strict = named(&contracts, "Both")?;
        let ContractKind::Struct { fields } = strict.kind() else {
            eyre::bail!("a struct");
        };
        assert!(fields.iter().all(|field| !field.is_capture()));
        assert!(matches!(
            fields
                .iter()
                .find(|field| field.wire_name == "name")
                .map(|f| &f.ty),
            Some(TypeRef::String)
        ));
    }

    /// A read form's struct can always be built from nothing, so it derives `Default` without
    /// being asked; the strict form does not unless the configuration asks.
    #[test_util::test]
    fn a_read_form_struct_derives_default() {
        let (contracts, _) = contracts_of(
            document("Sent", "Got", &schemas_sending_both()),
            &Config::default(),
        )?;
        let got = named(&contracts, "Got")?;
        assert!(
            got.derives().contains(&crate::config::Derive::Default),
            "{:?}",
            got.derives()
        );
        let sent = named(&contracts, "Sent")?;
        assert!(!sent.derives().contains(&crate::config::Derive::Default));
    }

    /// A schema that declares `additionalProperties` already has a capture member, and the read
    /// form does not add a second one.
    #[test_util::test]
    fn a_declared_capture_is_not_doubled() {
        let schemas = json!({
            "Got": {"type": "object", "required": ["id"],
                    "properties": {"id": {"type": "string"}},
                    "additionalProperties": {"type": "integer"}},
            "Sent": {"type": "object", "properties": {"id": {"type": "string"}}},
        });
        let (contracts, _) = contracts_of(document("Sent", "Got", &schemas), &Config::default())?;
        let got = named(&contracts, "Got")?;
        let ContractKind::Struct { fields } = got.kind() else {
            eyre::bail!("a struct");
        };
        let captures: Vec<_> = fields.iter().filter(|field| field.is_capture()).collect();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].capture, Some(super::Capture::Declared));
    }

    /// Strict decoding is the one-type-per-schema world: no forms, no twins, no read module.
    #[test_util::test]
    fn strict_decoding_assigns_no_forms() {
        let config = Config {
            decoding: Decoding::Strict,
            ..Config::default()
        };
        let (contracts, _) = contracts_of(document("Sent", "Got", &schemas()), &config)?;
        assert!(!contracts.has_read_forms());
        assert!(
            contracts
                .types()
                .iter()
                .all(|contract| contract.form() == Form::Strict { twin: None })
        );
        let got = named(&contracts, "Got")?;
        let ContractKind::Struct { fields } = got.kind() else {
            eyre::bail!("a struct");
        };
        assert!(fields.iter().all(|field| !field.is_capture()));
    }
}
