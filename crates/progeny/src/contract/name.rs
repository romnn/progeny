//! Rust identifiers, and what to do when two shapes want the same one.
//!
//! Sanitization is **total**: every wire name maps to some identifier, because a document is not
//! allowed to be unrenderable. Two rules matter more than they look:
//!
//! * A reserved word gets a trailing underscore (`type_`) rather than becoming a raw identifier.
//!   `r#type` would be prettier, but raw identifiers cannot express `self`, `crate` or `super`, so
//!   the uniform rule is the one that is actually total.
//! * The wire name is always kept in the contract. Renaming is a Rust-side concern; the bytes on
//!   the wire are decided by the document and nothing else.

use std::collections::BTreeMap;

use heck::{ToSnakeCase, ToUpperCamelCase};

/// A valid Rust identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RustIdent(String);

impl RustIdent {
    /// A type or variant name, from name segments.
    pub(crate) fn type_name(segments: &[String]) -> Self {
        let joined = segments.join(" ");
        Self(guard(&joined.to_upper_camel_case(), "Type"))
    }

    /// A field name.
    pub(crate) fn field(name: &str) -> Self {
        Self(guard(&name.to_snake_case(), "field"))
    }

    /// A function or method name, from name segments.
    pub(crate) fn method(segments: &[String]) -> Self {
        Self(guard(&segments.join(" ").to_snake_case(), "call"))
    }

    /// A variant name.
    pub(crate) fn variant(name: &str) -> Self {
        Self(guard(&name.to_upper_camel_case(), "Variant"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The same name with a numeric suffix, for disambiguation.
    fn suffixed(&self, index: u32) -> Self {
        Self(format!("{}{index}", self.0))
    }
}

impl std::fmt::Display for RustIdent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Make a case-converted string into something Rust will accept.
///
/// `fallback` is used when there is nothing left to work with — a property named `"-"` or `""`,
/// which real documents do contain.
fn guard(converted: &str, fallback: &str) -> String {
    let mut cleaned: String = converted
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    if cleaned.is_empty() {
        cleaned = fallback.to_owned();
    }
    if cleaned.starts_with(|character: char| character.is_ascii_digit()) {
        cleaned.insert(0, '_');
    }
    if RESERVED.contains(&cleaned.as_str()) || SHADOWED.contains(&cleaned.as_str()) {
        cleaned.push('_');
    }
    cleaned
}

/// Every word Rust will not accept as an identifier.
///
/// The 2015, 2018 and 2021 keyword sets together, plus the reserved-for-future-use ones: a name
/// that compiles today and breaks on an edition bump is not a name worth emitting. `self`, `crate`
/// and `super` are in the list and are exactly why the rule is a suffix rather than `r#`.
const RESERVED: [&str; 51] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "try", "type",
    "typeof", "unsafe", "unsized", "use", "virtual", "where", "while",
];

/// Every name the generated source itself writes unqualified.
///
/// Rust accepts these as identifiers, which is exactly the problem: a component named `Vec` becomes
/// `pub type Vec = Vec<Collection>;`, where the second `Vec` resolves to the first and rustc
/// reports a cycle. `chroma` publishes precisely that. Everything the renderer can qualify it does
/// — maps are `std::collections::BTreeMap`, arbitrary JSON is `serde_json::Value` — so this list is
/// the residue that has nowhere to be qualified to: the prelude names and the primitives.
///
/// A suffix rather than a qualification because the collision is with a *name we chose to use*, and
/// renaming ours would move the problem to whichever type is unlucky enough to be next.
const SHADOWED: [&str; 12] = [
    "Box", "Option", "Result", "String", "Vec", "bool", "char", "f32", "f64", "i64", "str", "u64",
];

/// Hands out type names, keeping them unique.
#[derive(Debug, Default)]
pub(crate) struct Namer {
    taken: BTreeMap<String, u32>,
}

/// A name, and whether it had to be changed to stay unique.
pub(crate) struct Named {
    pub(crate) ident: RustIdent,
    /// The name the segments asked for, when it was taken already.
    pub(crate) collided_with: Option<RustIdent>,
}

impl Namer {
    /// Claim a name built from these segments.
    ///
    /// On a collision the longer context is tried first — `PetFriend` before `PetFriend2` — because
    /// a name that says where it came from is worth more than a short one, and only then a counter.
    pub(crate) fn claim(&mut self, segments: &[String]) -> Named {
        let wanted = RustIdent::type_name(segments);
        if self.take(&wanted) {
            return Named {
                ident: wanted,
                collided_with: None,
            };
        }
        for index in 2..u32::MAX {
            let candidate = wanted.suffixed(index);
            if self.take(&candidate) {
                return Named {
                    ident: candidate,
                    collided_with: Some(wanted),
                };
            }
        }
        // Unreachable in practice: `u32::MAX` distinct suffixes for one name would need a document
        // no machine can hold. Returning the wanted name keeps this total.
        Named {
            ident: wanted,
            collided_with: None,
        }
    }

    /// Make an identifier unique among the ones already handed out, without reporting it.
    ///
    /// For names that are only unique *within* one item — a struct's fields, an enum's variants —
    /// where two wire names converting to one identifier is a fact about that item and not a
    /// collision anyone can act on.
    pub(crate) fn unique(&mut self, ident: RustIdent) -> RustIdent {
        if self.take(&ident) {
            return ident;
        }
        for index in 2..u32::MAX {
            let candidate = ident.suffixed(index);
            if self.take(&candidate) {
                return candidate;
            }
        }
        ident
    }

    fn take(&mut self, candidate: &RustIdent) -> bool {
        let slot = self.taken.entry(candidate.as_str().to_owned()).or_default();
        *slot += 1;
        *slot == 1
    }
}

/// The name segments a schema's address contributes, past the root that names it.
///
/// Structural keywords become a readable word rather than disappearing: dropping them would make
/// `properties/a/items` and `properties/a/additionalProperties` ask for the same name, and a
/// collision repaired by a counter is worse than a name that says `Item` or `Value`.
pub(crate) fn segment(token: &str, previous: Option<&str>) -> Option<String> {
    // The token after one of these is a name or an index that says everything worth saying.
    if matches!(
        previous,
        Some(
            "properties"
                | "allOf"
                | "anyOf"
                | "oneOf"
                | "patternProperties"
                | "dependentSchemas"
                | "$defs"
                | "definitions"
                | "prefixItems"
        )
    ) {
        return Some(token.to_owned());
    }
    match token {
        "properties" | "allOf" | "anyOf" | "oneOf" | "patternProperties" | "dependentSchemas"
        | "$defs" | "definitions" | "prefixItems" => None,
        "items" | "contains" | "unevaluatedItems" => Some("item".to_owned()),
        "additionalProperties" | "unevaluatedProperties" => Some("value".to_owned()),
        "propertyNames" => Some("key".to_owned()),
        "contentSchema" => Some("content".to_owned()),
        "not" | "if" | "then" | "else" => Some(token.to_owned()),
        // A media type, a status code, a path template: all worth keeping, all needing sanitizing,
        // which the identifier constructor does.
        other => Some(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Namer, RustIdent, segment};

    #[test]
    fn names_are_case_converted() {
        assert_eq!(
            RustIdent::type_name(&["pet".to_owned(), "friend".to_owned()]).as_str(),
            "PetFriend"
        );
        assert_eq!(RustIdent::field("petName").as_str(), "pet_name");
        assert_eq!(RustIdent::field("pet-name").as_str(), "pet_name");
        assert_eq!(RustIdent::variant("in-progress").as_str(), "InProgress");
    }

    #[test]
    fn reserved_words_get_a_suffix_rather_than_a_raw_identifier() {
        assert_eq!(RustIdent::field("type").as_str(), "type_");
        // The case `r#` cannot express, which is why the rule is uniform.
        assert_eq!(RustIdent::field("self").as_str(), "self_");
        assert_eq!(RustIdent::field("Self").as_str(), "self_");
        assert_eq!(RustIdent::type_name(&["type".to_owned()]).as_str(), "Type");
    }

    #[test]
    fn a_name_the_generated_source_already_uses_gets_out_of_its_own_way() {
        // `chroma` publishes a component called `Vec`, which produced
        // `pub type Vec = Vec<Collection>;` — accepted as an identifier, then rejected by rustc as
        // a cycle, because the second `Vec` resolves to the first.
        assert_eq!(RustIdent::type_name(&["Vec".to_owned()]).as_str(), "Vec_");
        assert_eq!(
            RustIdent::type_name(&["Option".to_owned()]).as_str(),
            "Option_"
        );
        assert_eq!(
            RustIdent::type_name(&["String".to_owned()]).as_str(),
            "String_"
        );
        // A name that merely starts the same way is not a collision.
        assert_eq!(
            RustIdent::type_name(&["Vector".to_owned()]).as_str(),
            "Vector"
        );
    }

    #[test]
    fn sanitization_is_total() {
        assert_eq!(RustIdent::field("").as_str(), "field");
        assert_eq!(RustIdent::field("-").as_str(), "field");
        assert_eq!(RustIdent::field("2fa").as_str(), "_2fa");
        assert_eq!(RustIdent::type_name(&[]).as_str(), "Type");
        assert_eq!(RustIdent::variant("200").as_str(), "_200");
        assert_eq!(RustIdent::field("x-rate-limit").as_str(), "x_rate_limit");
        // Non-ASCII names have no identifier form progeny will guess at.
        assert_eq!(RustIdent::field("日本語").as_str(), "field");
    }

    #[test]
    fn a_collision_is_disambiguated_deterministically() {
        let mut namer = Namer::default();
        let first = namer.claim(&["Pet".to_owned()]);
        let second = namer.claim(&["Pet".to_owned()]);
        let third = namer.claim(&["Pet".to_owned()]);
        assert_eq!(first.ident.as_str(), "Pet");
        assert!(first.collided_with.is_none());
        assert_eq!(second.ident.as_str(), "Pet2");
        assert_eq!(
            second.collided_with.as_ref().map(RustIdent::as_str),
            Some("Pet")
        );
        assert_eq!(third.ident.as_str(), "Pet3");
    }

    #[test]
    fn two_names_that_sanitize_alike_still_collide() {
        let mut namer = Namer::default();
        assert_eq!(
            namer.claim(&["pet-kind".to_owned()]).ident.as_str(),
            "PetKind"
        );
        assert_eq!(
            namer.claim(&["pet_kind".to_owned()]).ident.as_str(),
            "PetKind2"
        );
    }

    #[test]
    fn structural_tokens_become_readable_words() {
        assert_eq!(segment("properties", None), None);
        assert_eq!(
            segment("friend", Some("properties")),
            Some("friend".to_owned())
        );
        assert_eq!(segment("items", None), Some("item".to_owned()));
        assert_eq!(
            segment("additionalProperties", None),
            Some("value".to_owned())
        );
        // An index under a subschema array is the only thing distinguishing its branches.
        assert_eq!(segment("0", Some("anyOf")), Some("0".to_owned()));
    }
}
