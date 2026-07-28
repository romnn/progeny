//! The lossless schema model: JSON Schema 2020-12 held as a superset.
//!
//! Its single job is to hold *everything the document says*, including keywords progeny does
//! not interpret, so that no information is destroyed before the layers that decide what to do
//! with it. Every unmatched member goes into [`SchemaObject::unknown`] verbatim; there is no
//! code path that discards one, and the corpus round-trip is the executable proof.
//!
//! Subschemas are [`SchemaId`]s into a [`SchemaStore`] rather than boxed children. An arena
//! plus copyable ids is the entire cycle story: a recursive schema is just an id that happens
//! to form a loop, needing no reference-counted graph, no clone storms, and no inline
//! expansion — which is why the predecessor's recursive-expansion failure mode cannot occur
//! here at all.

pub(crate) mod cycles;
pub(crate) mod parse;
// Reached only through the document serializer, which is itself only called by the round-trip
// check. See the note there.
#[cfg_attr(
    not(feature = "harness"),
    allow(
        dead_code,
        reason = "the round-trip check is the only caller so far, and it is feature-gated"
    )
)]
pub(crate) mod serialize;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Number, Value};

use crate::diag::JsonPointer;

/// A schema's address in the [`SchemaStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SchemaId(u32);

impl SchemaId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    /// The id as a graph node index, for the cycle analysis.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

/// A schema. `true` and `false` are legal schemas in 2020-12 and mean "accept anything" and
/// "accept nothing"; `additionalProperties: false` is the form the corpus is full of.
///
/// The object form is boxed because it is two orders of magnitude larger than the boolean one
/// and large specs hold tens of thousands of schemas.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Schema {
    Bool(bool),
    Object(Box<SchemaObject>),
}

/// The arena every schema lives in.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SchemaStore {
    schemas: Vec<Schema>,
    /// Where each schema was written, parallel to `schemas`.
    ///
    /// Recorded at parse time because it is the only moment the position is known, and it earns
    /// its keep three times over: resolution turns a `$ref` into an id by matching addresses,
    /// diagnostics point at documents rather than at ids, and a generated type's name stays
    /// stable under unrelated edits because it derives from the address rather than from visit
    /// order.
    addresses: Vec<JsonPointer>,
}

/// The schema an out-of-store id degrades to.
///
/// Ids are minted only by [`SchemaStore::insert`], so a missing one cannot occur. Should it
/// ever, "accept anything" is the sound answer: it never rejects a payload the document
/// allows.
static ANY: Schema = Schema::Bool(true);

impl SchemaStore {
    pub(crate) fn insert(&mut self, schema: Schema, at: JsonPointer) -> SchemaId {
        // A store larger than `u32::MAX` schemas would need a document larger than any
        // machine can load; saturating keeps the id total instead of panicking on overflow.
        let id = SchemaId(u32::try_from(self.schemas.len()).unwrap_or(u32::MAX));
        self.schemas.push(schema);
        self.addresses.push(at);
        id
    }

    pub(crate) fn get(&self, id: SchemaId) -> &Schema {
        self.schemas.get(id.index()).unwrap_or(&ANY)
    }

    /// Where the schema was written. The root pointer for an id from no store, which is the same
    /// answer [`SchemaStore::get`] gives: an impossible id degrades rather than panicking.
    pub(crate) fn address(&self, id: SchemaId) -> &JsonPointer {
        static ROOT: OnceLock<JsonPointer> = OnceLock::new();
        self.addresses
            .get(id.index())
            .unwrap_or_else(|| ROOT.get_or_init(JsonPointer::root))
    }

    pub(crate) fn len(&self) -> usize {
        self.schemas.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (SchemaId, &Schema)> {
        self.schemas
            .iter()
            .enumerate()
            .map(|(index, schema)| (SchemaId(u32::try_from(index).unwrap_or(u32::MAX)), schema))
    }
}

/// A grammar position where the document may write either one value or an array of them.
///
/// `type: "string"` and `type: ["string"]` mean the same thing and must serialize back the way
/// they were written, so which form was used is data rather than a detail lost in parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read by the corpus harness, which is feature-gated"
        )
    )]
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        match self {
            Self::One(value) => std::slice::from_ref(value).iter(),
            Self::Many(values) => values.iter(),
        }
    }
}

/// A JSON Schema type name.
///
/// [`TypeName::Other`] exists because the model is a superset: real documents write
/// `type: "file"` and worse, and holding the spelling verbatim is what lets the shape layer
/// decide between repairing a known alias and degrading — a decision it cannot make from a
/// member that was thrown away at parse time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TypeName {
    Null,
    Boolean,
    Object,
    Array,
    Number,
    String,
    Integer,
    Other(String),
}

impl TypeName {
    pub(crate) fn parse(text: &str) -> Self {
        match text {
            "null" => Self::Null,
            "boolean" => Self::Boolean,
            "object" => Self::Object,
            "array" => Self::Array,
            "number" => Self::Number,
            "string" => Self::String,
            "integer" => Self::Integer,
            other => Self::Other(other.to_owned()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Number => "number",
            Self::String => "string",
            Self::Integer => "integer",
            Self::Other(text) => text,
        }
    }
}

/// Everything one schema object can say.
///
/// Numeric keywords are held as [`Number`], not as `u64`/`f64`: a bound is stored, not
/// interpreted, so `maxLength: 5` and `maxLength: 5.0` stay distinguishable and a
/// 128-bit `maximum` survives. Interpretation happens later, through accessors that can
/// diagnose what they cannot use.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SchemaObject {
    pub(crate) schema_dialect: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) dynamic_anchor: Option<String>,
    /// Held as the string the document wrote. Resolution to a [`SchemaId`] is a separate
    /// phase, so that parsing stays total and a dangling reference is a diagnosable finding
    /// rather than a parse failure.
    pub(crate) reference: Option<String>,
    pub(crate) dynamic_reference: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) defs: Option<BTreeMap<String, SchemaId>>,
    /// draft-04's spelling of `$defs`, which is not a 2020-12 keyword and is written by every
    /// tool that lowered a JSON Schema into an OpenAPI document.
    ///
    /// Modelled rather than left among the uninterpreted members for one concrete reason: real
    /// documents *reference into* it. `codat-accounting` writes 72 distinct
    /// `#/components/schemas/X/definitions/Y` references, and a keyword held verbatim has no
    /// address for those to resolve to, so all of them would degrade to "accept anything". The
    /// normalizer already descends into it; this is what makes the parser agree.
    pub(crate) definitions: Option<BTreeMap<String, SchemaId>>,

    pub(crate) all_of: Option<Vec<SchemaId>>,
    pub(crate) any_of: Option<Vec<SchemaId>>,
    pub(crate) one_of: Option<Vec<SchemaId>>,
    pub(crate) not: Option<SchemaId>,
    pub(crate) if_schema: Option<SchemaId>,
    pub(crate) then_schema: Option<SchemaId>,
    pub(crate) else_schema: Option<SchemaId>,
    pub(crate) dependent_schemas: Option<BTreeMap<String, SchemaId>>,
    pub(crate) properties: Option<BTreeMap<String, SchemaId>>,
    pub(crate) pattern_properties: Option<BTreeMap<String, SchemaId>>,
    pub(crate) additional_properties: Option<SchemaId>,
    pub(crate) property_names: Option<SchemaId>,
    pub(crate) items: Option<SchemaId>,
    pub(crate) prefix_items: Option<Vec<SchemaId>>,
    pub(crate) contains: Option<SchemaId>,
    pub(crate) unevaluated_items: Option<SchemaId>,
    pub(crate) unevaluated_properties: Option<SchemaId>,

    pub(crate) types: Option<OneOrMany<TypeName>>,
    pub(crate) enumeration: Option<Vec<Value>>,
    pub(crate) constant: Option<Value>,
    pub(crate) multiple_of: Option<Number>,
    pub(crate) maximum: Option<Number>,
    pub(crate) exclusive_maximum: Option<Number>,
    pub(crate) minimum: Option<Number>,
    pub(crate) exclusive_minimum: Option<Number>,
    pub(crate) max_length: Option<Number>,
    pub(crate) min_length: Option<Number>,
    pub(crate) pattern: Option<String>,
    pub(crate) max_items: Option<Number>,
    pub(crate) min_items: Option<Number>,
    pub(crate) unique_items: Option<bool>,
    pub(crate) max_contains: Option<Number>,
    pub(crate) min_contains: Option<Number>,
    pub(crate) max_properties: Option<Number>,
    pub(crate) min_properties: Option<Number>,
    pub(crate) required: Option<Vec<String>>,
    pub(crate) dependent_required: Option<BTreeMap<String, Vec<String>>>,

    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) default: Option<Value>,
    pub(crate) deprecated: Option<bool>,
    pub(crate) read_only: Option<bool>,
    pub(crate) write_only: Option<bool>,
    pub(crate) examples: Option<Vec<Value>>,
    /// Held verbatim: `format` is an annotation rather than an assertion, so which formats
    /// mean something is the type model's business.
    pub(crate) format: Option<String>,
    pub(crate) content_encoding: Option<String>,
    pub(crate) content_media_type: Option<String>,
    pub(crate) content_schema: Option<SchemaId>,

    pub(crate) discriminator: Option<Discriminator>,
    pub(crate) xml: Option<Xml>,
    pub(crate) external_docs: Option<ExternalDocs>,

    /// Every member progeny does not model, preserved verbatim: `x-*` vendor extensions,
    /// keywords from a dialect that does not exist yet, and members whose value had the wrong
    /// shape.
    pub(crate) unknown: BTreeMap<String, Value>,
}

impl SchemaObject {
    /// Every subschema this one holds, in a fixed order.
    ///
    /// The one place that knows the shape of the schema graph: resolution builds its edge list
    /// from this, the shape layer bounds its traversals with it, and a field added to
    /// [`SchemaObject`] that is not listed here would make a subschema invisible to both. The
    /// `EVERY_KEYWORD` fixture in the serializer's tests pins the coverage, by asserting that
    /// walking from the root reaches every schema the store holds.
    pub(crate) fn children(&self, mut visit: impl FnMut(SchemaId)) {
        for id in [
            self.not,
            self.if_schema,
            self.then_schema,
            self.else_schema,
            self.additional_properties,
            self.property_names,
            self.items,
            self.contains,
            self.unevaluated_items,
            self.unevaluated_properties,
            self.content_schema,
        ]
        .into_iter()
        .flatten()
        {
            visit(id);
        }
        for group in [&self.all_of, &self.any_of, &self.one_of, &self.prefix_items] {
            for &id in group.iter().flatten() {
                visit(id);
            }
        }
        for group in [
            &self.defs,
            &self.definitions,
            &self.dependent_schemas,
            &self.properties,
            &self.pattern_properties,
        ] {
            for (_, &id) in group.iter().flatten() {
                visit(id);
            }
        }
    }
}

/// The OpenAPI discriminator: which property names the variant, and how names map to schemas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Discriminator {
    pub(crate) property_name: Option<String>,
    pub(crate) mapping: Option<BTreeMap<String, String>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// The OpenAPI XML object. Carried losslessly and not interpreted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Xml {
    pub(crate) name: Option<String>,
    pub(crate) namespace: Option<String>,
    pub(crate) prefix: Option<String>,
    pub(crate) attribute: Option<bool>,
    pub(crate) wrapped: Option<bool>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// A pointer to documentation outside the document. Appears both inside schemas and at several
/// document levels, and the *layering* is what fixes the home: `doc` sits above `schema` and may
/// import it, never the reverse, so the one shared node has to live on the schema side however
/// lopsided its use counts are (today four document positions to one schema position).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExternalDocs {
    pub(crate) description: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::{JsonPointer, OneOrMany, Schema, SchemaId, SchemaStore, TypeName};

    #[test]
    fn ids_address_what_was_inserted() {
        let mut store = SchemaStore::default();
        let at = JsonPointer::root().child("components").child("schemas");
        let yes = store.insert(Schema::Bool(true), at.child("Yes"));
        let no = store.insert(Schema::Bool(false), at.child("No"));
        assert_ne!(yes, no);
        assert_eq!(store.get(yes), &Schema::Bool(true));
        assert_eq!(store.get(no), &Schema::Bool(false));
        assert_eq!(store.len(), 2);
        assert_eq!(store.address(yes).to_string(), "/components/schemas/Yes");
    }

    #[test]
    fn an_id_from_no_store_degrades_to_accept_anything() {
        let store = SchemaStore::default();
        let mut other = SchemaStore::default();
        let stranger = other.insert(Schema::Bool(false), JsonPointer::root());
        assert_eq!(store.get(stranger), &Schema::Bool(true));
        assert!(store.address(stranger).is_root());
    }

    #[test]
    fn one_and_many_iterate_the_same_way() {
        let one: OneOrMany<TypeName> = OneOrMany::One(TypeName::String);
        let many = OneOrMany::Many(vec![TypeName::String]);
        assert_eq!(
            one.iter().collect::<Vec<_>>(),
            many.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn unrecognized_type_names_keep_their_spelling() {
        assert_eq!(TypeName::parse("file"), TypeName::Other("file".to_owned()));
        assert_eq!(TypeName::parse("file").as_str(), "file");
        for name in [
            "null", "boolean", "object", "array", "number", "string", "integer",
        ] {
            assert_eq!(TypeName::parse(name).as_str(), name);
        }
    }

    #[test]
    fn schema_ids_are_copyable_so_cycles_are_free() {
        let mut store = SchemaStore::default();
        let id: SchemaId = store.insert(Schema::Bool(true), JsonPointer::root());
        let copy = id;
        assert_eq!(id, copy);
    }
}
