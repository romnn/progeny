//! Which types the caller's open rules land on: the types a selector names, and every generated
//! type those reach.
//!
//! The closure is the point. A derive or a trait impl on a struct does not compile unless every
//! type the struct holds has one too, so a rule naming one type is a rule about everything under
//! it — fields, list items and map values, union variants, the anonymous types written inline, and
//! the read forms a read form names. Computing that from the type graph here means a caller names
//! the type they care about and never maintains a list that rots when the description grows a
//! member.

use std::collections::BTreeSet;

use super::lower::Provisional;
use super::{ContractKind, Form, Gate, GatedDerives};
use crate::config::{Config, Decoding, FormFilter, Selector, SerdeAttributes, TypeSelection};
use crate::diag::{RejectError, RejectKind};

/// What the open rules decided for each type, indexed like the provisional contracts.
#[derive(Debug)]
pub(super) struct Extensions {
    /// The open derives each type carries, one entry per rule that reached it.
    pub(super) derives: Vec<Vec<GatedDerives>>,
    /// Whether each type gets a `schemars::JsonSchema` impl, and behind what.
    pub(super) json_schema: Vec<Option<Gate>>,
    /// Whether a rule's derives read serde attributes on this type, so it takes the serde derive
    /// path whatever the configured strategy is.
    pub(super) attributes_read: Vec<bool>,
}

/// Apply every open rule in the configuration.
///
/// # Errors
///
/// Returns a rejection when a selector names nothing, names a form the type does not have, when a
/// rule whose derives read serde attributes reaches a kind those attributes cannot describe, or
/// when two rules put one derive on one type behind different gates — both gates could be on at
/// once, and the two copies would conflict.
pub(super) fn run(types: &[Provisional], config: &Config) -> Result<Extensions, RejectError> {
    let mut extensions = Extensions {
        derives: vec![Vec::new(); types.len()],
        json_schema: vec![None; types.len()],
        attributes_read: vec![false; types.len()],
    };
    for rule in &config.extra_derives {
        let gate = Gate::of(rule.feature.as_deref());
        for index in closure(types, &rule.types, rule.nested, config, "extra-derives")? {
            let Some(contract) = types.get(index) else {
                continue;
            };
            // An alias is a name for another type, so it has no item to put a derive on; the
            // closure already went through it to what it names.
            if matches!(contract.kind, ContractKind::Alias { .. }) {
                continue;
            }
            if rule.serde_attributes == SerdeAttributes::Read {
                attributes_describe(contract)?;
                if let Some(read) = extensions.attributes_read.get_mut(index) {
                    *read = true;
                }
            }
            let Some(carried) = extensions.derives.get_mut(index) else {
                continue;
            };
            let mut paths = Vec::new();
            for path in &rule.derives {
                if let Some(earlier) = carried.iter().find(|it| it.paths.contains(path)) {
                    if earlier.gate == gate {
                        continue;
                    }
                    return Err(RejectError::new(
                        RejectKind::UnsatisfiableConfig,
                        format!(
                            "two `extra-derives` rules put `{path}` on `{}` behind different \
                             gates; with both on, the two derives would conflict, so give them \
                             one gate",
                            contract.rust_name
                        ),
                    )
                    .at(contract.origin.clone()));
                }
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
            if !paths.is_empty() {
                carried.push(GatedDerives {
                    gate: gate.clone(),
                    paths,
                });
            }
        }
    }
    if let Some(schemas) = &config.json_schema {
        let gate = Gate::of(schemas.feature.as_deref());
        // Always nested: an impl on the outer type does not compile without one on each type it
        // holds, and these are progeny's to write.
        for index in closure(types, &schemas.types, true, config, "json-schema")? {
            let alias = types
                .get(index)
                .is_none_or(|contract| matches!(contract.kind, ContractKind::Alias { .. }));
            if let Some(slot) = extensions.json_schema.get_mut(index)
                && !alias
            {
                *slot = Some(gate.clone());
            }
        }
    }
    Ok(extensions)
}

/// Refuse a kind whose wire format no `#[serde(...)]` attribute set describes.
///
/// Both take a hand-written serde path under every configuration, for exactly that reason, so the
/// attributes an attribute-reading derive would see there are none at all.
fn attributes_describe(contract: &Provisional) -> Result<(), RejectError> {
    let why = match contract.kind {
        ContractKind::StringEnum { .. } => {
            "an open string enum keeps an unlisted value verbatim, which no serde attribute \
             encodes"
        }
        ContractKind::CarriedTagEnum { .. } => {
            "its variants carry their own tag member, which no serde attribute encodes"
        }
        ContractKind::Struct { .. }
        | ContractKind::Enum { .. }
        | ContractKind::TaggedEnum { .. }
        | ContractKind::Newtype { .. }
        | ContractKind::Tuple { .. }
        | ContractKind::Alias { .. } => return Ok(()),
    };
    Err(RejectError::new(
        RejectKind::UnsatisfiableConfig,
        format!(
            "an `extra-derives` rule whose derives read serde attributes reaches `{}`, and {why}; \
             such a derive would describe a different type than the wire carries",
            contract.rust_name
        ),
    )
    .at(contract.origin.clone()))
}

/// Every type a selection names, and, when `nested`, every generated type those reach.
///
/// Not nested, an alias is still walked through to what it names: it has no item of its own, so
/// selecting one means selecting the type behind it.
fn closure(
    types: &[Provisional],
    selection: &TypeSelection,
    nested: bool,
    config: &Config,
    table: &str,
) -> Result<BTreeSet<usize>, RejectError> {
    let keys = match selection {
        TypeSelection::All => return Ok((0..types.len()).collect()),
        TypeSelection::Only(keys) => keys,
    };
    let mut pending = Vec::new();
    for key in keys {
        let selector = Selector::parse(key);
        let named: Vec<usize> = types
            .iter()
            .enumerate()
            .filter(|(_, contract)| {
                selector
                    .address
                    .names(contract.component.as_deref(), &contract.origin.to_string())
            })
            .map(|(index, _)| index)
            .collect();
        if named.is_empty() {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                format!(
                    "the configuration's `{table}` selects `{key}`, which is neither a component \
                     name nor the JSON Pointer of any generated type in this document"
                ),
            ));
        }
        let chosen: Vec<usize> = named
            .into_iter()
            .filter(|&index| {
                types
                    .get(index)
                    .is_some_and(|contract| in_form(selector.form, contract.form))
            })
            .collect();
        if chosen.is_empty() {
            return Err(RejectError::new(
                RejectKind::UnsatisfiableConfig,
                missing_form(table, key, selector.form, config),
            ));
        }
        pending.extend(chosen);
    }
    let mut reached = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if !reached.insert(index) {
            continue;
        }
        if let Some(contract) = types.get(index)
            && (nested || matches!(contract.kind, ContractKind::Alias { .. }))
        {
            let mut named = Vec::new();
            for ty in contract.kind.references() {
                ty.named(&mut named);
            }
            pending.extend(named.into_iter().map(super::TypeIndex::index));
        }
    }
    Ok(reached)
}

/// Whether a contract of this form is what the qualifier asks for.
fn in_form(filter: FormFilter, form: Form) -> bool {
    match filter {
        FormFilter::Every => true,
        // What `types::read::Name` names: a read twin, a response-only type, or a shared type the
        // module re-exports.
        FormFilter::Read => form.is_read(),
        // What `types::Name` names.
        FormFilter::Strict => !form.is_lenient(),
    }
}

/// Why a qualified selector found the type and not the form it asked for.
fn missing_form(table: &str, key: &str, filter: FormFilter, config: &Config) -> String {
    match (filter, config.decoding) {
        (FormFilter::Read, Decoding::Strict) => format!(
            "the configuration's `{table}` selects `{key}`, but strict decoding generates no \
             read forms; drop the `read::` qualifier"
        ),
        (FormFilter::Read, Decoding::Lenient) => format!(
            "the configuration's `{table}` selects `{key}`, but no response reaches that type, \
             so it has no read form; drop the `read::` qualifier"
        ),
        (FormFilter::Strict | FormFilter::Every, _) => format!(
            "the configuration's `{table}` selects `{key}`, but only responses reach that type, \
             so it lives in `types::read` alone; write `read::` instead of `strict::`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::super::tests::{contracts_of, with_schemas};
    use super::super::{Contracts, DeserStrategy, Gate, TypeContract};
    use crate::config::{
        Config, Decoding, ExtraDerives, JsonSchemaImpls, SerdeAttributes, TypeSelection,
    };

    /// A document with one operation sending `Sent` and answering with `Got`; `Both` is reached
    /// from both, and has a required member, so it twins.
    fn twinned() -> Value {
        json!({
            "openapi": "3.1.0",
            "paths": {"/things": {"post": {
                "operationId": "postThing",
                "requestBody": {"content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Sent"},
                }}},
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Got"},
                }}}},
            }}},
            "components": {"schemas": {
                "Sent": {"type": "object", "properties": {"both": {"$ref": "#/components/schemas/Both"}}},
                "Got": {"type": "object", "properties": {"both": {"$ref": "#/components/schemas/Both"}}},
                "Both": {"type": "object", "required": ["name"],
                         "properties": {"name": {"type": "string"},
                                        "note": {"$ref": "#/components/schemas/Note"}}},
                "Note": {"type": "object", "properties": {"text": {"type": "string"}}},
            }},
        })
    }

    fn schemas_for(selectors: &[&str]) -> Config {
        Config {
            json_schema: Some(JsonSchemaImpls {
                types: TypeSelection::Only(selectors.iter().map(|&it| it.to_owned()).collect()),
                feature: Some("schemars".to_owned()),
            }),
            ..Config::default()
        }
    }

    fn derives_for(selectors: &[&str], serde_attributes: SerdeAttributes) -> Config {
        Config {
            extra_derives: vec![ExtraDerives {
                derives: vec!["::fixtures::Fixture".to_owned()],
                types: TypeSelection::Only(selectors.iter().map(|&it| it.to_owned()).collect()),
                nested: true,
                feature: Some("fixtures".to_owned()),
                serde_attributes,
            }],
            ..Config::default()
        }
    }

    /// Every type that got a JSON Schema impl, as `name` or `read::name`.
    fn with_schema(contracts: &Contracts) -> Vec<String> {
        contracts
            .types()
            .iter()
            .filter(|contract| contract.json_schema().is_some())
            .map(spelled)
            .collect()
    }

    fn spelled(contract: &TypeContract) -> String {
        if contract.form().is_lenient() {
            format!("read::{}", contract.rust_name())
        } else {
            contract.rust_name().to_string()
        }
    }

    fn rejection(document: Value, config: &Config) -> eyre::Result<String> {
        let error = contracts_of(document, config)
            .err()
            .ok_or_eyre("the configuration must be refused")?;
        Ok(error.to_string())
    }

    /// The closure runs through every way one type holds another: a member, a list item, a map
    /// value, a union variant, an anonymous type written inline, and an alias — which gets no
    /// impl, having no item to carry one, while what it names does.
    #[test_util::test]
    fn a_selected_type_brings_everything_it_reaches_and_nothing_else() {
        let (contracts, _) = contracts_of(
            with_schemas(json!({
                "Holder": {"type": "object", "properties": {
                    "member": {"$ref": "#/components/schemas/Member"},
                    "items": {"type": "array", "items": {"$ref": "#/components/schemas/Item"}},
                    "values": {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/Value"}},
                    "choice": {"oneOf": [
                        {"$ref": "#/components/schemas/Left"},
                        {"type": "array", "items": {"type": "integer"}},
                    ]},
                    "inline": {"type": "object", "properties": {"deep": {"$ref": "#/components/schemas/Deep"}}},
                    "named": {"$ref": "#/components/schemas/Names"},
                }},
                "Member": {"type": "object", "properties": {"a": {"type": "string"}}},
                "Item": {"type": "object", "properties": {"b": {"type": "string"}}},
                "Value": {"type": "object", "properties": {"c": {"type": "string"}}},
                "Left": {"type": "object", "required": ["d"], "properties": {"d": {"type": "string"}}},
                "Deep": {"type": "string", "enum": ["x", "y"]},
                "Names": {"type": "array", "items": {"$ref": "#/components/schemas/Aliased"}},
                "Aliased": {"type": "object", "properties": {"e": {"type": "string"}}},
                "Unrelated": {"type": "object", "properties": {"f": {"type": "string"}}},
            })),
            &schemas_for(&["Holder"]),
        )?;
        let mut got = with_schema(&contracts);
        got.sort();
        assert_eq!(
            got,
            [
                "Aliased",
                "Deep",
                "Holder",
                "HolderChoice",
                "HolderInline",
                "Item",
                "Left",
                "Member",
                "Value",
            ]
        );
        let gate = contracts
            .types()
            .iter()
            .find_map(TypeContract::json_schema)
            .ok_or_eyre("a selected type")?;
        assert_eq!(*gate, Gate::Feature("schemars".to_owned()));
    }

    /// Both spellings of the one key grammar select the same type.
    #[test_util::test]
    fn a_selector_is_a_component_name_or_a_pointer() {
        let schemas = with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Other": {"type": "object", "properties": {"name": {"type": "integer"}}},
        }));
        for key in ["Pet", "/components/schemas/Pet"] {
            let (contracts, _) = contracts_of(schemas.clone(), &schemas_for(&[key]))?;
            assert_eq!(with_schema(&contracts), ["Pet"], "selected by `{key}`");
        }
    }

    /// `read::` and `strict::` select what `types::read::Name` and `types::Name` name; the bare
    /// name selects both forms of a twinned type, and each form brings the closure of its own
    /// graph — the read twin reaches the read forms, the strict one the strict forms.
    #[test_util::test]
    fn a_qualifier_selects_one_form_of_a_twinned_type() {
        let (read, _) = contracts_of(twinned(), &schemas_for(&["read::Both"]))?;
        let mut got = with_schema(&read);
        got.sort();
        assert_eq!(got, ["Note", "read::Both"]);

        let (strict, _) = contracts_of(twinned(), &schemas_for(&["strict::Both"]))?;
        let mut got = with_schema(&strict);
        got.sort();
        assert_eq!(got, ["Both", "Note"]);

        let (every, _) = contracts_of(twinned(), &schemas_for(&["Both"]))?;
        let mut got = with_schema(&every);
        got.sort();
        assert_eq!(got, ["Both", "Note", "read::Both"]);

        // A read form reaching a twinned type names the twin, never the strict half.
        let (holder, _) = contracts_of(twinned(), &schemas_for(&["read::Got"]))?;
        let mut got = with_schema(&holder);
        got.sort();
        assert_eq!(got, ["Note", "read::Both", "read::Got"]);
    }

    /// A qualifier naming a form the type does not have is refused, with the reason, rather than
    /// applying to nothing.
    #[test_util::test]
    fn a_form_the_type_does_not_have_is_refused() {
        let refused = rejection(twinned(), &schemas_for(&["read::Sent"]))?;
        assert!(refused.contains("no response reaches"), "{refused}");
        let refused = rejection(twinned(), &schemas_for(&["strict::Got"]))?;
        assert!(refused.contains("only responses reach"), "{refused}");
        let strict = Config {
            decoding: Decoding::Strict,
            ..schemas_for(&["read::Both"])
        };
        let refused = rejection(twinned(), &strict)?;
        assert!(refused.contains("strict decoding"), "{refused}");
        let refused = rejection(twinned(), &schemas_for(&["Bothh"]))?;
        assert!(refused.contains("`Bothh`"), "{refused}");
    }

    /// An open derive lands on the closure with its gate, and leaves the serde strategy alone
    /// when the caller rules that it ignores serde attributes.
    #[test_util::test]
    fn an_attribute_blind_derive_keeps_the_hand_written_path() {
        let (contracts, _) = contracts_of(
            twinned(),
            &derives_for(&["read::Got"], SerdeAttributes::Ignored),
        )?;
        let carrying: Vec<String> = contracts
            .types()
            .iter()
            .filter(|contract| !contract.extra_derives().is_empty())
            .map(spelled)
            .collect();
        assert_eq!(carrying, ["Note", "read::Got", "read::Both"]);
        for contract in contracts.types() {
            assert_ne!(
                contract.deser(),
                DeserStrategy::Derive,
                "{}",
                spelled(contract)
            );
            for rule in contract.extra_derives() {
                assert_eq!(rule.gate, Gate::Feature("fixtures".to_owned()));
                assert_eq!(rule.paths, ["::fixtures::Fixture"]);
            }
        }
    }

    /// Off, `nested` stops a rule at the types it selects, while the default reaches what they
    /// hold; a selected alias still passes the derive to the type it names.
    #[test_util::test]
    fn a_rule_that_is_not_nested_stops_at_the_selected_types() {
        let document = with_schemas(json!({
            "Holder": {"type": "object", "properties": {
                "child": {"$ref": "#/components/schemas/Child"},
            }},
            "Child": {"type": "object", "properties": {"name": {"type": "string"}}},
            "Children": {"type": "array", "items": {"$ref": "#/components/schemas/Child"}},
        }));
        let carrying = |selectors: &[&str], nested: bool| -> eyre::Result<Vec<String>> {
            let mut config = derives_for(selectors, SerdeAttributes::Ignored);
            for rule in &mut config.extra_derives {
                rule.nested = nested;
            }
            let (contracts, _) = contracts_of(document.clone(), &config)?;
            let mut got: Vec<String> = contracts
                .types()
                .iter()
                .filter(|contract| !contract.extra_derives().is_empty())
                .map(spelled)
                .collect();
            got.sort();
            Ok(got)
        };
        assert_eq!(carrying(&["Holder"], true)?, ["Child", "Holder"]);
        assert_eq!(carrying(&["Holder"], false)?, ["Holder"]);
        assert_eq!(carrying(&["Children"], false)?, ["Child"]);
    }

    /// A derive the caller says reads serde attributes moves exactly the types it lands on to the
    /// derive path, where those attributes are the wire contract.
    #[test_util::test]
    fn an_attribute_reading_derive_moves_its_types_to_the_serde_derive() {
        let (contracts, _) = contracts_of(
            twinned(),
            &derives_for(&["read::Got"], SerdeAttributes::Read),
        )?;
        for contract in contracts.types() {
            let reached = !contract.extra_derives().is_empty();
            assert_eq!(
                contract.deser() == DeserStrategy::Derive,
                reached,
                "{}",
                spelled(contract)
            );
        }
    }

    /// No attribute set describes an open string enum, so a derive that reads them cannot be told
    /// the truth about one and is refused.
    #[test_util::test]
    fn an_attribute_reading_derive_refuses_a_kind_attributes_cannot_describe() {
        let refused = rejection(
            with_schemas(json!({
                "Holder": {"type": "object", "properties": {"kind": {"$ref": "#/components/schemas/Kind"}}},
                "Kind": {"type": "string", "enum": ["a", "b"]},
            })),
            &derives_for(&["Holder"], SerdeAttributes::Read),
        )?;
        assert!(refused.contains("`Kind`"), "{refused}");
        assert!(refused.contains("open string enum"), "{refused}");
    }

    /// One derive behind two gates could be on twice, and two copies conflict.
    #[test_util::test]
    fn one_derive_behind_two_gates_on_one_type_is_refused() {
        let schemas = with_schemas(json!({
            "Pet": {"type": "object", "properties": {"name": {"type": "string"}}},
        }));
        let rule = |feature: &str| ExtraDerives {
            derives: vec!["::fixtures::Fixture".to_owned()],
            types: TypeSelection::All,
            nested: true,
            feature: Some(feature.to_owned()),
            serde_attributes: SerdeAttributes::Ignored,
        };
        let same = Config {
            extra_derives: vec![rule("fixtures"), rule("fixtures")],
            ..Config::default()
        };
        let (contracts, _) = contracts_of(schemas.clone(), &same)?;
        let pet = contracts.types().first().ok_or_eyre("the one type")?;
        assert_eq!(pet.extra_derives().len(), 1, "{:?}", pet.extra_derives());

        let different = Config {
            extra_derives: vec![rule("fixtures"), rule("testing")],
            ..Config::default()
        };
        let refused = rejection(schemas, &different)?;
        assert!(refused.contains("different gates"), "{refused}");
    }
}
