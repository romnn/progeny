//! The inheritance spelling of a family of schemas, read as inheritance rather than as a merge.
//!
//! A base lists its subtypes in a `oneOf` or `anyOf`, and each subtype extends the base through
//! `allOf`:
//!
//! ```yaml
//! Pet:
//!   type: object
//!   properties: {kind: {type: string}, name: {type: string}}
//!   discriminator: {propertyName: kind}
//!   oneOf: [{$ref: "#/components/schemas/Cat"}]
//! Cat:
//!   allOf:
//!     - $ref: "#/components/schemas/Pet"
//!     - properties: {lives: {type: integer}}
//! ```
//!
//! Merged as written, the two halves fold into each other.
//! `Cat` merges `Pet` whole, its `oneOf` included, so `Cat` is again the union of `Pet`'s
//! subtypes, and a family of one subtype is a type that is only itself.
//! What the document means is that a `Cat` holds `Pet`'s members and its own, and that a `Pet` is
//! one of its subtypes.
//! So a subtype folds its base in as an inherited base ([`Part::inherited_base`]): the base's own
//! members and constraints, without the union that dispatches to the subtypes or the
//! discriminator that tags them.
//! The base keeps both and is read as the union of its subtypes, by its tag where it declares a
//! discriminator, however few subtypes it has.
//!
//! The rule recognizes only families it reads with certainty, and leaves every other schema to
//! the merge:
//!
//! - every branch of the base's union refers to a subtype whose `allOf` refers back to the base,
//!   so the base is the union of its subtypes and of nothing else;
//! - the base refers to no schema of its own, through `$ref` or `allOf`, so its projection is the
//!   one schema it writes;
//! - the base's `additionalProperties`, if it writes one, is `true`, because `false` refuses and a
//!   schema types every member a subtype adds;
//! - no subtype heads a family of its own or extends a second schema with a union, where reading
//!   one family would decide another.

use std::collections::BTreeSet;

use super::merge;
use super::{Part, Reading, ShapeKey};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};
use crate::resolve::ResolvedDocument;
use crate::schema::{Schema, SchemaId};

/// What an `allOf` member of `child` contributes to its key.
///
/// The base the member refers back to, as an inherited base, where `child` is a subtype of a family
/// this rule reads, and the member read in full otherwise.
pub(super) fn part(resolved: &ResolvedDocument, child: SchemaId, member: SchemaId) -> Part {
    let Some(base) = reference(resolved, member) else {
        return Part::full(member);
    };
    match Family::of(resolved, base) {
        Some(family) if family.subtypes.iter().any(|subtype| subtype.id == child) => {
            Part::inherited_base(base)
        }
        _ => Part::full(member),
    }
}

/// Whether a key is the base of a family this rule reads, read in full.
pub(super) fn heads_family(resolved: &ResolvedDocument, key: &ShapeKey) -> bool {
    match key.parts() {
        [part] if !key.is_branches() && part.reading == Reading::Full => {
            Family::of(resolved, part.id).is_some()
        }
        _ => false,
    }
}

/// Report every family a classified key reads, once, at its base's union.
///
/// A family is reported wherever one of its subtypes was classified, which is wherever the rule
/// changed what a type is, whether or not anything reaches the base.
pub(super) fn report<'a>(
    resolved: &ResolvedDocument,
    keys: impl IntoIterator<Item = &'a ShapeKey>,
    ctx: &mut Ctx,
) {
    let bases: BTreeSet<SchemaId> = keys
        .into_iter()
        .flat_map(ShapeKey::parts)
        .filter(|part| part.reading == Reading::InheritedBase)
        .map(|part| part.id)
        .collect();
    for base in bases {
        let Some(family) = Family::of(resolved, base) else {
            continue;
        };
        let addresses = resolved.schemas();
        let keyword = family.keyword;
        let listed = match family.subtypes.len() {
            1 => format!("the one schema this `{keyword}` lists extends"),
            count => format!("each of the {count} schemas this `{keyword}` lists extends"),
        };
        ctx.report(
            Diagnostic::new(
                BreakageClass::InheritanceCycle,
                Action::Repair,
                addresses.address(base).child(keyword),
                format!(
                    "{listed} this schema through `allOf`, so a subtype merged with it whole would \
                     hold this `{keyword}` and be one of its own variants; each subtype is read as \
                     this schema's members and its own, without this `{keyword}`, and this schema \
                     as the union of its subtypes"
                ),
            )
            .with_related(
                family
                    .subtypes
                    .iter()
                    .map(|subtype| addresses.address(subtype.site).clone()),
            ),
        );
    }
}

/// A base and the subtypes its union lists, each extending it through `allOf`.
struct Family {
    /// The keyword the base lists its subtypes under, `oneOf` or `anyOf`.
    keyword: &'static str,
    /// In the order the union lists them.
    subtypes: Vec<Subtype>,
}

/// One subtype of a family.
struct Subtype {
    id: SchemaId,
    /// The member of the subtype's `allOf` that refers back to the base.
    site: SchemaId,
}

impl Family {
    /// The family `base` heads, when it is one this rule reads.
    fn of(resolved: &ResolvedDocument, base: SchemaId) -> Option<Self> {
        let Schema::Object(object) = resolved.schema(base) else {
            return None;
        };
        // A base that writes both keywords is two unions, which no family reading settles.
        let (keyword, branches) = match (&object.one_of, &object.any_of) {
            (Some(branches), None) => ("oneOf", branches),
            (None, Some(branches)) => ("anyOf", branches),
            _ => return None,
        };
        // An `additionalProperties` other than `true` refuses or types every member a subtype
        // adds, which the merge applies only to extras no field names, so the projection would
        // not hold the base faithfully.
        let undeclared = object
            .additional_properties
            .is_some_and(|rest| !matches!(resolved.schema(rest), Schema::Bool(true)));
        if undeclared || object.all_of.is_some() || resolved.follow(base).is_some() {
            return None;
        }
        // A branch that is not a subtype makes the family mixed, and the merge reads it.
        let subtypes = branches
            .iter()
            .map(|&branch| {
                let id = reference(resolved, branch)?;
                let site = extends(resolved, id, base)?;
                Some(Subtype { id, site })
            })
            .collect::<Option<Vec<_>>>()?;
        (!subtypes.is_empty()).then_some(Self { keyword, subtypes })
    }
}

/// The member of `subtype`'s `allOf` that refers to `base`, where `subtype` extends `base` and no
/// other schema with a union.
fn extends(resolved: &ResolvedDocument, subtype: SchemaId, base: SchemaId) -> Option<SchemaId> {
    let Schema::Object(object) = resolved.schema(subtype) else {
        return None;
    };
    // A subtype with a union of its own heads a family of its own, and one that refers elsewhere
    // merges whatever it refers to.
    if declares_union(resolved, subtype) || resolved.follow(subtype).is_some() {
        return None;
    }
    let mut site = None;
    for &member in object.all_of.iter().flatten() {
        match reference(resolved, member) {
            Some(target) if target == base => site = site.or(Some(member)),
            // A second schema with a union is a second family the subtype would belong to.
            Some(target) if declares_union(resolved, target) => return None,
            _ => {}
        }
    }
    site
}

/// The schema a bare reference names: one that writes `$ref` and says nothing else about the
/// value.
fn reference(resolved: &ResolvedDocument, id: SchemaId) -> Option<SchemaId> {
    let Schema::Object(object) = resolved.schema(id) else {
        return None;
    };
    if merge::constrains(object) || object.all_of.is_some() {
        return None;
    }
    resolved.follow(id)
}

fn declares_union(resolved: &ResolvedDocument, id: SchemaId) -> bool {
    matches!(resolved.schema(id), Schema::Object(object)
        if object.one_of.is_some() || object.any_of.is_some())
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use crate::diag::{Action, BreakageClass, Diagnostic};
    use crate::shape::tests::{shape_of, shapes_of, with_schemas};
    use crate::shape::{Shape, ShapeRef, Shapes, TagStyle, Union};

    /// A subtype of `Base` adding the member `own`.
    fn subtype(own: &str) -> Value {
        json!({"allOf": [
            {"$ref": "#/components/schemas/Base"},
            {"type": "object", "properties": {own: {"type": "string"}}},
        ]})
    }

    /// A base declaring `kind` and `name`, listing `Child` under `keyword`, tagged by `kind` where
    /// `discriminated`.
    fn base(keyword: &str, discriminated: bool) -> Value {
        let mut base = json!({
            "type": "object",
            "required": ["kind"],
            "properties": {"kind": {"type": "string"}, "name": {"type": "string"}},
        });
        base[keyword] = json!([{"$ref": "#/components/schemas/Child"}]);
        if discriminated {
            base["discriminator"] = json!({
                "propertyName": "kind",
                "mapping": {"child": "#/components/schemas/Child"},
            });
        }
        base
    }

    /// The wire names of a struct's fields, in the order it declares them.
    fn fields(shape: &Shape) -> eyre::Result<Vec<&str>> {
        let Shape::Struct(structure) = shape else {
            eyre::bail!("expected a struct, found {shape:?}");
        };
        Ok(structure
            .fields
            .iter()
            .map(|field| field.wire.as_str())
            .collect())
    }

    /// The union a shape is, with its tag.
    fn tagged(shape: &Shape) -> eyre::Result<&Union> {
        match shape {
            Shape::Union(union) if union.tag.is_some() => Ok(union),
            other => eyre::bail!("expected a tagged union, found {other:?}"),
        }
    }

    /// The shape each variant of a union holds.
    fn variants<'a>(shapes: &'a Shapes, union: &Union) -> eyre::Result<Vec<&'a Shape>> {
        union
            .variants
            .iter()
            .map(|variant| match &variant.shape {
                ShapeRef::Key(key) => shapes.get(key).ok_or_eyre("every variant is classified"),
                ShapeRef::Inline(_) => eyre::bail!("a subtype is a schema of its own"),
            })
            .collect()
    }

    fn families(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
        diagnostics
            .iter()
            .filter(|found| found.class() == BreakageClass::InheritanceCycle)
            .collect()
    }

    /// A base listing its one subtype, tagged, is a tagged union of that subtype, and the subtype
    /// is a struct of the base's members and its own, where the merge made it an alias of itself.
    #[test_util::test]
    fn a_discriminated_family_of_one_is_a_tagged_union_of_a_struct() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base("oneOf", true),
            "Child": subtype("endpoint"),
        })))?;

        // The base dispatches by its tag, consumed because nothing else uses the subtype.
        let union = tagged(shape_of(&shapes, "Base")?)?;
        let tags: Vec<Option<&str>> = union
            .variants
            .iter()
            .map(|variant| variant.tag.as_deref())
            .collect();
        assert_eq!(tags, [Some("child")]);
        assert_eq!(
            union.tag.as_ref().map(|tag| tag.style),
            Some(TagStyle::Consumed)
        );

        // The subtype holds the base's members and its own, the consumed tag aside, and is the
        // type the union's one variant holds.
        let child = shape_of(&shapes, "Child")?;
        assert_eq!(fields(child)?, ["endpoint", "name"]);
        assert_eq!(variants(&shapes, union)?, [child]);

        // One repair, at the base's union, naming the subtype's reference back.
        let [family] = families(&diagnostics)[..] else {
            eyre::bail!("one family is one record: {diagnostics:#?}");
        };
        assert_eq!(family.action(), Action::Repair);
        assert_eq!(
            family.location().to_string(),
            "/components/schemas/Base/oneOf"
        );
        let related: Vec<String> = family.related().iter().map(ToString::to_string).collect();
        assert_eq!(related, ["/components/schemas/Child/allOf/0"]);
    }

    /// Several subtypes are each a struct of the base's members and their own, and the base is the
    /// union its tag names.
    /// A `oneOf` elsewhere over the same subtypes still reads the tag the base declares, which
    /// none of them takes as a dispatch of its own.
    #[test_util::test]
    fn a_family_of_several_is_a_tagged_union_of_structs() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"type": "string"}, "name": {"type": "string"}},
                "discriminator": {
                    "propertyName": "kind",
                    "mapping": {
                        "cat": "#/components/schemas/Cat",
                        "dog": "#/components/schemas/Dog",
                    },
                },
                "oneOf": [
                    {"$ref": "#/components/schemas/Cat"},
                    {"$ref": "#/components/schemas/Dog"},
                ],
            },
            "Cat": subtype("lives"),
            "Dog": subtype("pack"),
            "Sighting": {
                "type": "object",
                "properties": {"seen": {"oneOf": [
                    {"$ref": "#/components/schemas/Cat"},
                    {"$ref": "#/components/schemas/Dog"},
                ]}},
            },
        })))?;

        let union = tagged(shape_of(&shapes, "Base")?)?;
        let tags: Vec<Option<&str>> = union
            .variants
            .iter()
            .map(|variant| variant.tag.as_deref())
            .collect();
        assert_eq!(tags, [Some("cat"), Some("dog")]);
        assert_eq!(fields(shape_of(&shapes, "Cat")?)?, ["lives", "name"]);
        assert_eq!(fields(shape_of(&shapes, "Dog")?)?, ["name", "pack"]);

        // The member's union over the family dispatches by the tag the base maps too.
        let Shape::Struct(sighting) = shape_of(&shapes, "Sighting")? else {
            eyre::bail!("`Sighting` is a struct");
        };
        let ShapeRef::Key(seen) = &sighting.fields[0].shape else {
            eyre::bail!("the member is a named union");
        };
        let seen = tagged(shapes.get(seen).ok_or_eyre("the union is classified")?)?;
        assert_eq!(
            seen.tag.as_ref().map(|tag| tag.property.as_str()),
            Some("kind")
        );

        let [family] = families(&diagnostics)[..] else {
            eyre::bail!("one family is one record: {diagnostics:#?}");
        };
        let related: Vec<String> = family.related().iter().map(ToString::to_string).collect();
        assert_eq!(
            related,
            [
                "/components/schemas/Cat/allOf/0",
                "/components/schemas/Dog/allOf/0"
            ]
        );
    }

    /// A base listing its one subtype without a tag is an alias of that subtype, which is a struct.
    #[test_util::test]
    fn an_undiscriminated_family_of_one_is_an_alias_of_its_subtype() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base("oneOf", false),
            "Child": subtype("endpoint"),
        })))?;
        let child = shape_of(&shapes, "Child")?;
        assert_eq!(fields(child)?, ["endpoint", "kind", "name"]);
        let Shape::Alias(ShapeRef::Key(target)) = shape_of(&shapes, "Base")? else {
            eyre::bail!("the base is an alias");
        };
        assert_eq!(shapes.get(target), Some(child));
        assert_eq!(families(&diagnostics).len(), 1, "{diagnostics:#?}");
    }

    /// A family listed under `anyOf` is read as one listed under `oneOf`, and reported there.
    #[test_util::test]
    fn a_family_listed_under_any_of_is_read_the_same() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base("anyOf", true),
            "Child": subtype("endpoint"),
        })))?;
        tagged(shape_of(&shapes, "Base")?)?;
        assert_eq!(fields(shape_of(&shapes, "Child")?)?, ["endpoint", "name"]);
        let [family] = families(&diagnostics)[..] else {
            eyre::bail!("one family is one record: {diagnostics:#?}");
        };
        assert_eq!(
            family.location().to_string(),
            "/components/schemas/Base/anyOf"
        );
    }

    /// A subtype that binds the tag to constants of its own is named by them, as the branch of
    /// any union that names itself is, rather than by its component name, which the constant
    /// forbids it to carry.
    #[test_util::test]
    fn subtypes_that_bind_the_tag_are_named_by_their_constants() {
        let kind = |own: &str, value: &str| {
            json!({"allOf": [
                {"$ref": "#/components/schemas/Base"},
                {"properties": {"kind": {"const": value}, own: {"type": "integer"}}},
            ]})
        };
        let (shapes, _) = shapes_of(with_schemas(json!({
            "Base": {
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"type": "string"}},
                "discriminator": {"propertyName": "kind"},
                "oneOf": [
                    {"$ref": "#/components/schemas/Cat"},
                    {"$ref": "#/components/schemas/Dog"},
                ],
            },
            "Cat": kind("lives", "cat"),
            "Dog": kind("pack", "dog"),
            // A family of one, whose lone subtype is named by its constant too.
            "Solo": {
                "type": "object",
                "properties": {"kind": {"type": "string"}},
                "discriminator": {"propertyName": "kind"},
                "oneOf": [{"$ref": "#/components/schemas/Bird"}],
            },
            "Bird": {"allOf": [
                {"$ref": "#/components/schemas/Solo"},
                {"properties": {"kind": {"const": "bird"}}},
            ]},
        })))?;
        let spelled = |name: &str| -> eyre::Result<Vec<Option<String>>> {
            Ok(tagged(shape_of(&shapes, name)?)?
                .variants
                .iter()
                .map(|variant| variant.tag.clone())
                .collect())
        };
        assert_eq!(
            spelled("Base")?,
            [Some("cat".to_owned()), Some("dog".to_owned())]
        );
        assert_eq!(spelled("Solo")?, [Some("bird".to_owned())]);
        // The union writes the tag it consumes, so the subtype holds only its own member.
        assert_eq!(fields(shape_of(&shapes, "Cat")?)?, ["lives"]);
    }

    /// A base whose `additionalProperties` is a schema says what every member a subtype adds is,
    /// which the merge applies only to members no field names, so the family is left to it.
    #[test_util::test]
    fn a_base_typing_its_undeclared_members_is_left_to_the_merge() {
        let mut base = base("oneOf", true);
        base["additionalProperties"] = json!({
            "type": "object",
            "required": ["baseName"],
            "properties": {"baseName": {"type": "string"}},
        });
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base,
            "Child": subtype("endpoint"),
        })))?;
        assert!(
            !matches!(shape_of(&shapes, "Child")?, Shape::Struct(_)),
            "the subtype merges the base whole"
        );
        assert!(families(&diagnostics).is_empty(), "{diagnostics:#?}");
    }

    /// A union listing a schema that is not a subtype beside one that is is not a family, so the
    /// subtype merges the base whole and keeps the degradation that has.
    #[test_util::test]
    fn a_mixed_family_is_left_to_the_merge() {
        let mut base = base("oneOf", false);
        base["oneOf"] = json!([
            {"$ref": "#/components/schemas/Child"},
            {"$ref": "#/components/schemas/Other"},
        ]);
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base,
            "Child": subtype("endpoint"),
            "Other": {"type": "object", "properties": {"size": {"type": "integer"}}},
        })))?;
        assert_eq!(shape_of(&shapes, "Child")?, &Shape::Any);
        assert!(families(&diagnostics).is_empty(), "{diagnostics:#?}");
    }

    /// A subtype extending a second schema with a union of its own belongs to two families, and
    /// reading one would decide the other, so neither is read.
    #[test_util::test]
    fn a_subtype_of_two_families_is_left_to_the_merge() {
        let (shapes, diagnostics) = shapes_of(with_schemas(json!({
            "Base": base("oneOf", true),
            "Second": {
                "type": "object",
                "properties": {"tier": {"type": "string"}},
                "oneOf": [{"$ref": "#/components/schemas/Child"}],
            },
            "Child": {"allOf": [
                {"$ref": "#/components/schemas/Base"},
                {"$ref": "#/components/schemas/Second"},
                {"type": "object", "properties": {"endpoint": {"type": "string"}}},
            ]},
        })))?;
        assert!(
            !matches!(shape_of(&shapes, "Child")?, Shape::Struct(_)),
            "the subtype merges both bases whole"
        );
        assert!(families(&diagnostics).is_empty(), "{diagnostics:#?}");
    }

    /// A subtype an operation answers with directly carries its tag on the wire there, so the
    /// family's union leaves the tag on the subtype rather than taking it off.
    #[test_util::test]
    fn a_subtype_used_directly_keeps_the_tag_its_family_carries() {
        let mut document = with_schemas(json!({
            "Base": base("oneOf", true),
            "Child": subtype("endpoint"),
        }));
        document["paths"] = json!({"/child": {"get": {
            "operationId": "getChild",
            "responses": {"200": {
                "description": "ok",
                "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Child"},
                }},
            }},
        }}});
        let (shapes, _) = shapes_of(document)?;
        let union = tagged(shape_of(&shapes, "Base")?)?;
        assert_eq!(
            union.tag.as_ref().map(|tag| tag.style),
            Some(TagStyle::Carried)
        );
        assert_eq!(
            fields(shape_of(&shapes, "Child")?)?,
            ["endpoint", "kind", "name"]
        );
    }
}
