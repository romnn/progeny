//! Whether a JSON value could be a value a shape describes.
//!
//! One rule with two callers, deliberately. The contract layer asks it about every declared
//! `default` before keeping one, and the API layer asks it about every example the surface
//! carries — and the two used to ask *different* checkers, one shallow over `TypeRef` and one
//! recursive over shapes, which could disagree about the same JSON: a document whose `default`
//! and `example` were the same wrong object literal was told about the example and not the
//! default. A rule that lives below both layers cannot disagree with itself.
//!
//! The verdict is deliberately one-sided: it reports only what is certainly wrong. A shape progeny
//! degraded to [`Shape::Any`] accepts everything, an absent constraint constrains nothing, and both
//! answer "no mismatch" — because both callers act on a "no": the contract layer drops the default,
//! and the payload harness distrusts the example. An uncertain "maybe" would be a value dropped, or
//! a false failure reported, for nothing progeny actually knows.
//!
//! Constraints progeny does not turn into a type — `minLength`, `pattern`, `multipleOf` — are not
//! checked, because a value violating one still deserializes into the generated type, which is the
//! only property either caller depends on.

use serde_json::Value;

use super::{Extra, Scalar, Shape, ShapeRef, Shapes, Struct, Union};

/// The checker: the shape table, and the word the reasons call the value being judged.
pub(crate) struct Fit<'a> {
    shapes: &'a Shapes,
    /// "the example", "the default" — whatever noun the caller's diagnostic speaks in, so the
    /// rule stays one while each finding names the thing it is actually about.
    subject: &'static str,
}

impl<'a> Fit<'a> {
    pub(crate) fn new(shapes: &'a Shapes, subject: &'static str) -> Self {
        Self { shapes, subject }
    }

    /// Why a value cannot be what a shape describes, if it cannot.
    pub(crate) fn mismatch(&self, value: &Value, shape: &Shape) -> Option<String> {
        match shape {
            Shape::Any => None,
            Shape::Null => (!value.is_null())
                .then(|| format!("the schema says null, {} is {}", self.subject, kind(value))),
            Shape::Optional(inner) => {
                if value.is_null() {
                    return None;
                }
                self.through(value, inner)
            }
            Shape::Alias(inner) => self.through(value, inner),
            Shape::Scalar(scalar) => self.scalar(value, *scalar),
            // A format is a string with a spelling rule progeny does not check here.
            Shape::Format(_) => (!value.is_string()).then(|| {
                format!(
                    "the schema says a string, {} is {}",
                    self.subject,
                    kind(value)
                )
            }),
            // Only reached when every listed value is a string — a mixed `enum` degrades to `Any`
            // long before this — so the generated type is a unit-variant enum that reads from a
            // string and nothing else. A non-string here is as certainly wrong as a wrong spelling.
            Shape::StringEnum(values) => {
                let Some(text) = value.as_str() else {
                    return Some(format!(
                        "the schema says a string, {} is {}",
                        self.subject,
                        kind(value)
                    ));
                };
                (!values.iter().any(|allowed| allowed == text)).then(|| {
                    format!(
                        "`{text}` is not one of the {} values the enum allows",
                        values.len()
                    )
                })
            }
            Shape::Struct(structure) => self.structure(value, structure),
            Shape::Map { value: element } => self.mapping(value, element.as_ref()),
            // Recursing into elements matters for the same reason it matters into a struct's
            // members, and the containers were the half that was missing: all three payloads
            // `github` could not deserialize are contradictions one element deep — a string where
            // the schema says an integer, an option missing a required member, a union branch
            // nothing accepts. A check that stops at the container calls each example sound, and
            // the payload gate then reports the vendor's defect as progeny's.
            Shape::Array { item } => self.list(value, item.as_ref()),
            Shape::FixedArray { item, len } => self.fixed(value, item, *len),
            Shape::Tuple { items, .. } => self.tuple(value, items),
            Shape::Union(union) => self.union(value, union),
        }
    }

    /// The same question through a reference: resolve the key, then ask.
    pub(crate) fn through(&self, value: &Value, reference: &ShapeRef) -> Option<String> {
        match reference {
            ShapeRef::Key(key) => self.mismatch(value, self.shapes.get(key)?),
            ShapeRef::Inline(shape) => self.mismatch(value, shape),
        }
    }

    fn structure(&self, value: &Value, structure: &Struct) -> Option<String> {
        let Value::Object(object) = value else {
            return Some(self.not_an_object(value));
        };
        for field in &structure.fields {
            let Some(present) = object.get(&field.wire) else {
                if field.required {
                    return Some(format!(
                        "{} leaves out `{}`, which the schema requires",
                        self.subject, field.wire
                    ));
                }
                continue;
            };
            // Recursing matters more than it looks: a contradiction three members deep is still a
            // contradiction, and the payload gate's verdict for the whole example turns on it.
            // `cloudflare` writes `false` where its own schema says a string, inside a nested
            // object — a top-level check calls that example sound and the gate then reports the
            // vendor's defect as progeny's.
            if let Some(reason) = self.through(present, &field.shape) {
                return Some(format!("at `{}`, {reason}", field.wire));
            }
        }
        // `additionalProperties: false` lowers to a deserializer that refuses a member the schema
        // did not name, so an object carrying one is certainly not a value of this shape. The
        // untagged unions the payload gate prunes against turn on exactly this: `github`'s
        // `merge-async` details are three closed branches, and a rule blind to the closure reads
        // `{message, sha}` as the message-only branch — which serde rejects, taking the branch
        // that keeps `sha` and leaving the gate to report the surviving member as invented.
        if structure.extra == Extra::Denied
            && let Some(undeclared) = object
                .keys()
                .find(|key| !structure.fields.iter().any(|field| field.wire == **key))
        {
            return Some(format!(
                "{} carries `{undeclared}`, which the schema does not declare and \
                 `additionalProperties: false` forbids",
                self.subject
            ));
        }
        None
    }

    fn mapping(&self, value: &Value, element: Option<&ShapeRef>) -> Option<String> {
        let Value::Object(members) = value else {
            return Some(self.not_an_object(value));
        };
        // A map the document left untyped constrains its values not at all.
        let element = element?;
        for (key, member) in members {
            if let Some(reason) = self.through(member, element) {
                return Some(format!("at `{key}`, {reason}"));
            }
        }
        None
    }

    fn list(&self, value: &Value, item: Option<&ShapeRef>) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(self.not_an_array(value));
        };
        // A list the document left untyped constrains its elements not at all.
        self.uniform(items, item?)
    }

    fn fixed(&self, value: &Value, item: &ShapeRef, len: u32) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(self.not_an_array(value));
        };
        if items.len() != len as usize {
            return Some(format!(
                "the schema says {len} elements, {} has {}",
                self.subject,
                items.len()
            ));
        }
        self.uniform(items, item)
    }

    /// `rest` is deliberately not consulted: a tuple lowers to a Rust tuple whether or not the
    /// document allows elements past the prefix, and serde reads one only from an array of exactly
    /// that length — so a longer example fails to deserialize even where the schema permits it.
    fn tuple(&self, value: &Value, positions: &[ShapeRef]) -> Option<String> {
        let Value::Array(items) = value else {
            return Some(self.not_an_array(value));
        };
        if items.len() != positions.len() {
            return Some(format!(
                "the schema says {} elements, {} has {}",
                positions.len(),
                self.subject,
                items.len()
            ));
        }
        for (index, (element, position)) in items.iter().zip(positions).enumerate() {
            if let Some(reason) = self.through(element, position) {
                return Some(format!("at `{index}`, {reason}"));
            }
        }
        None
    }

    /// A union accepts whatever any branch accepts, and progeny already refuses to emit one whose
    /// branches nothing tells apart — so "no branch accepts this" is the only sound finding, and it
    /// needs every branch checked rather than any one of them.
    fn union(&self, value: &Value, union: &Union) -> Option<String> {
        let mut reasons = Vec::new();
        for variant in &union.variants {
            let reason = self.through(value, &variant.shape)?;
            reasons.push(reason);
        }
        (!reasons.is_empty())
            .then(|| format!("no branch of the union accepts it ({})", reasons.join("; ")))
    }

    /// Why one of these elements is not what the element shape describes, if one is not.
    fn uniform(&self, items: &[Value], item: &ShapeRef) -> Option<String> {
        for (index, element) in items.iter().enumerate() {
            if let Some(reason) = self.through(element, item) {
                return Some(format!("at `{index}`, {reason}"));
            }
        }
        None
    }

    fn scalar(&self, value: &Value, scalar: Scalar) -> Option<String> {
        let ok = match scalar {
            Scalar::Bool => value.is_boolean(),
            // An integer schema with a `1.0` example is the document being loose about JSON's one
            // number type, not a contradiction: it deserializes.
            Scalar::Integer { .. } => value.as_f64().is_some_and(|number| number.fract() == 0.0),
            Scalar::Number => value.is_number(),
            Scalar::String => value.is_string(),
        };
        (!ok).then(|| {
            format!(
                "the schema says {}, {} is {}",
                match scalar {
                    Scalar::Bool => "a boolean",
                    Scalar::Integer { .. } => "an integer",
                    Scalar::Number => "a number",
                    Scalar::String => "a string",
                },
                self.subject,
                kind(value)
            )
        })
    }

    fn not_an_array(&self, value: &Value) -> String {
        format!(
            "the schema says an array, {} is {}",
            self.subject,
            kind(value)
        )
    }

    fn not_an_object(&self, value: &Value) -> String {
        format!(
            "the schema says an object, {} is {}",
            self.subject,
            kind(value)
        )
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
