//! A list of values beside a union, narrowed to the values the union admits.
//!
//! `{enum: [a, b], anyOf: [{const: a}, {type: integer}]}` admits `"a"` alone: a value has to satisfy
//! the list and the union both.
//! The list is the tighter bound, so the schema reads as that list, minus the values a branch
//! provably refuses, and its union is not classified at all.
//! Without this, the union's type would stand in for the schema, and it reads values the list
//! leaves out.
//!
//! Admission is decided only where it can be decided exactly, from `type`, `enum`, `const`, the
//! boolean schemas and nested unions.
//! A value some other keyword might refuse, such as `pattern`, is kept, and so is the list as a
//! whole when it holds an object or a list, whose admission would take a general validator.
//! Either way the view says so, and the classifier reports the list as a restriction the generated
//! type does not hold.

use serde_json::Value;

use super::ShapeKey;
use crate::resolve::ResolvedDocument;
use crate::schema::{Schema, SchemaObject, TypeName};

/// How deep into nested unions admission is followed before it answers "cannot tell".
const MAX_DEPTH: usize = 4;

/// What a list beside a union narrows to.
#[derive(Debug, PartialEq)]
pub(super) enum Narrowed {
    /// The listed values the union admits or might admit, in the list's order.
    ///
    /// `checked` is false when some kept value could not be shown admitted.
    Values { values: Vec<Value>, checked: bool },
    /// No listed value is admitted, so the schema admits nothing.
    Nothing,
    /// The union is one branch and perhaps `null`, and admits listed values alone, so it already
    /// holds the list, and its type, a reference to a named type included, is the schema's.
    Held,
    /// The list holds an object or a list, which is not checked against the union.
    Unchecked,
}

/// Narrow `listed` to the values the union whose branches are `branches` admits.
///
/// `exactly_one` is `oneOf`'s rule, under which a value two branches admit is refused.
/// `types` is the `type` the schema itself declares, which a value has to satisfy too.
pub(super) fn narrow(
    resolved: &ResolvedDocument,
    listed: &[Value],
    types: Option<&std::collections::BTreeSet<TypeName>>,
    branches: &[ShapeKey],
    exactly_one: bool,
) -> Narrowed {
    if listed
        .iter()
        .any(|value| value.is_object() || value.is_array())
    {
        return Narrowed::Unchecked;
    }
    let mut values = Vec::new();
    let mut checked = true;
    for value in listed {
        let declared = types.map_or(Admission::Yes, |types| typed(types, value));
        let admitted = declared.and(union_admits(resolved, branches, exactly_one, value, 0));
        match admitted {
            Admission::Yes => values.push(value.clone()),
            Admission::Unknown => {
                checked = false;
                values.push(value.clone());
            }
            Admission::No => {}
        }
    }
    if values.is_empty() {
        Narrowed::Nothing
    } else if checked && aliases(resolved, branches, &values) {
        Narrowed::Held
    } else {
        Narrowed::Values { values, checked }
    }
}

/// Whether a union is one branch and perhaps `null`, every branch admitting listed values alone.
///
/// That is the form OpenAPI 3.0's `enum` beside a nullable reference to an enum takes once
/// normalized, and reading the list there would trade the reference for a copy of its values.
/// A union of two or more other branches is read as the list instead, whose one type tells its
/// values apart where branches of one kind would not.
fn aliases(resolved: &ResolvedDocument, branches: &[ShapeKey], values: &[Value]) -> bool {
    let nulls = branches
        .iter()
        .filter(|branch| is_null(resolved, branch))
        .count();
    // A `null` branch makes the kept union an `Option`, which is the schema's only where the
    // kept values include `null`.
    let null_kept = nulls == 0 || values.iter().any(Value::is_null);
    branches.len() - nulls == 1
        && null_kept
        && branches
            .iter()
            .all(|branch| holds_only(resolved, branch, values))
}

/// Whether a branch is the `null` type, the one branch a union's reading peels off as an
/// `Option` rather than counting it.
fn is_null(resolved: &ResolvedDocument, branch: &ShapeKey) -> bool {
    branch.parts().iter().any(|&part| {
        matches!(resolved.schema(part), Schema::Object(object)
        if object.types.as_ref().is_some_and(|types| {
            types.iter().all(|name| *name == TypeName::Null)
        }))
    })
}

/// Whether a branch admits nothing but the given values: one of its parts lists values of its own,
/// or is the `null` type, and each of those is among them.
///
/// A list that restates its union's values, as vapi writes one, and the `enum` OpenAPI 3.0 puts
/// beside a nullable reference to an enum, are held this way.
/// It reads the branch's parts as written, rather than their merged view, so a branch that refers
/// back to the schema cannot recurse.
fn holds_only(resolved: &ResolvedDocument, branch: &ShapeKey, values: &[Value]) -> bool {
    branch.parts().iter().any(|&part| {
        let object = match resolved.schema(part) {
            Schema::Bool(admits) => return !*admits,
            Schema::Object(object) => object,
        };
        // An empty list is no list, as the merged view reads one.
        let listed = object
            .enumeration
            .as_ref()
            .filter(|values| !values.is_empty());
        let own = match (listed, &object.constant) {
            (Some(listed), _) => listed.clone(),
            (None, Some(constant)) => vec![constant.clone()],
            (None, None) => {
                let null = object
                    .types
                    .as_ref()
                    .is_some_and(|types| types.iter().all(|name| *name == TypeName::Null));
                if !null {
                    return false;
                }
                vec![Value::Null]
            }
        };
        own.iter()
            .all(|value| among(values, value) == Admission::Yes)
    })
}

/// Whether a schema admits a value, as far as its keywords decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Yes,
    No,
    /// A keyword this does not evaluate may refuse the value.
    Unknown,
}

impl Admission {
    /// Both must admit: a refusal wins, then an open question.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Yes, Self::Yes) => Self::Yes,
        }
    }
}

/// Whether a union admits a value: one branch for `anyOf`, exactly one for `oneOf`.
fn union_admits(
    resolved: &ResolvedDocument,
    branches: &[ShapeKey],
    exactly_one: bool,
    value: &Value,
    depth: usize,
) -> Admission {
    let answers: Vec<Admission> = branches
        .iter()
        .map(|branch| admits(resolved, branch, value, depth))
        .collect();
    let yes = answers
        .iter()
        .filter(|answer| **answer == Admission::Yes)
        .count();
    let unknown = answers.contains(&Admission::Unknown);
    match (exactly_one, yes, unknown) {
        // Two branches that both admit it break `oneOf`, whatever the rest say, and no branch
        // admitting it refuses it under either.
        (true, 2.., _) | (_, 0, false) => Admission::No,
        (true, 1, false) | (false, 1.., _) => Admission::Yes,
        _ => Admission::Unknown,
    }
}

/// Whether every part of a key admits a value.
fn admits(resolved: &ResolvedDocument, key: &ShapeKey, value: &Value, depth: usize) -> Admission {
    if depth > MAX_DEPTH {
        return Admission::Unknown;
    }
    key.parts().iter().fold(Admission::Yes, |admission, &part| {
        admission.and(match resolved.schema(part) {
            Schema::Bool(admits) => {
                if *admits {
                    Admission::Yes
                } else {
                    Admission::No
                }
            }
            Schema::Object(object) => object_admits(resolved, object, value, depth),
        })
    })
}

/// Whether one schema object admits a value.
///
/// Its `$ref` and `allOf` are parts of the key already, so they are not followed here.
fn object_admits(
    resolved: &ResolvedDocument,
    object: &SchemaObject,
    value: &Value,
    depth: usize,
) -> Admission {
    let mut admission = if unevaluated(object, value) {
        Admission::Unknown
    } else {
        Admission::Yes
    };
    if let Some(types) = &object.types {
        admission = admission.and(typed(types.iter(), value));
    }
    // An empty list is no list, as the merged view reads one.
    if let Some(values) = object
        .enumeration
        .as_ref()
        .filter(|values| !values.is_empty())
    {
        admission = admission.and(among(values, value));
    }
    if let Some(constant) = &object.constant {
        admission = admission.and(among(std::slice::from_ref(constant), value));
    }
    for (branches, exactly_one) in [(&object.any_of, false), (&object.one_of, true)] {
        if let Some(branches) = branches {
            let keys: Vec<ShapeKey> = branches
                .iter()
                .map(|&id| super::merge::key_of(resolved, id))
                .collect();
            admission = admission.and(union_admits(resolved, &keys, exactly_one, value, depth + 1));
        }
    }
    admission
}

/// Whether a keyword this does not evaluate applies to a value of this kind.
///
/// Keywords for objects and lists say nothing about a string, a number, a boolean or `null`.
fn unevaluated(object: &SchemaObject, value: &Value) -> bool {
    let anywhere = object.not.is_some()
        || object.if_schema.is_some()
        || object.then_schema.is_some()
        || object.else_schema.is_some();
    let string = value.is_string()
        && (object.pattern.is_some() || object.min_length.is_some() || object.max_length.is_some());
    let number = value.is_number()
        && (object.minimum.is_some()
            || object.maximum.is_some()
            || object.exclusive_minimum.is_some()
            || object.exclusive_maximum.is_some()
            || object.multiple_of.is_some());
    anywhere || string || number
}

/// Whether a value is of one of the named types.
fn typed<'a>(types: impl IntoIterator<Item = &'a TypeName>, value: &Value) -> Admission {
    let mut admission = Admission::No;
    for name in types {
        match holds(name, value) {
            Some(true) => return Admission::Yes,
            Some(false) => {}
            None => admission = Admission::Unknown,
        }
    }
    admission
}

/// Whether a value is of a named type, or `None` where that cannot be told: for a name progeny
/// does not know, and for a number whose exponent hides whether it is whole.
fn holds(name: &TypeName, value: &Value) -> Option<bool> {
    Some(match name {
        TypeName::Null => value.is_null(),
        TypeName::Boolean => value.is_boolean(),
        TypeName::String => value.is_string(),
        TypeName::Number => value.is_number(),
        // An integer is a number with no fractional part, however it is spelled.
        TypeName::Integer => {
            let Value::Number(number) = value else {
                return Some(false);
            };
            match decimal(number) {
                Some(text) => !text.contains('.'),
                // An exponent hides whether the value is whole unless its reading has a fraction.
                None if number
                    .as_f64()
                    .is_some_and(|reading| reading.fract() != 0.0) =>
                {
                    false
                }
                None => return None,
            }
        }
        TypeName::Object => value.is_object(),
        TypeName::Array => value.is_array(),
        TypeName::Other(_) => return None,
    })
}

/// Whether two scalars are the same JSON value, numbers compared by what they are, so `2` is
/// `2.0`, or `None` where that cannot be told.
///
/// Numbers are compared by their decimal text, which is exact however long it is.
/// One written with an exponent is compared by its `f64` reading instead, which tells two numbers
/// apart but cannot show them equal.
#[expect(
    clippy::float_cmp,
    reason = "two readings that differ at all are two different numbers, so the comparison is exact \
              on purpose"
)]
pub(super) fn same(left: &Value, right: &Value) -> Option<bool> {
    let (Value::Number(left), Value::Number(right)) = (left, right) else {
        return Some(left == right);
    };
    if let (Some(left), Some(right)) = (decimal(left), decimal(right)) {
        return Some(left == right);
    }
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) if left != right => Some(false),
        _ => None,
    }
}

/// A number's value as decimal text, with no sign on zero and no trailing fractional zeros, or
/// `None` for one written with an exponent.
fn decimal(number: &serde_json::Number) -> Option<String> {
    let text = number.to_string();
    if text.contains(['e', 'E']) {
        return None;
    }
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text.as_str()), |digits| (true, digits));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let whole = match whole.trim_start_matches('0') {
        "" => "0",
        whole => whole,
    };
    let fraction = fraction.trim_end_matches('0');
    let sign = if negative && (whole != "0" || !fraction.is_empty()) {
        "-"
    } else {
        ""
    };
    Some(if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    })
}

/// Whether a value is among the listed ones, by [`same`].
fn among(listed: &[Value], value: &Value) -> Admission {
    let mut admission = Admission::No;
    for candidate in listed {
        match same(candidate, value) {
            Some(true) => return Admission::Yes,
            Some(false) => {}
            None => admission = Admission::Unknown,
        }
    }
    admission
}

/// The `type` a list of scalars has: the kind of each value, with `integer` for the numbers only
/// where each is written as one the generated integer holds and the schema's own `type` allows
/// it.
///
/// The integer is a `u64` where `unsigned` and an `i64` otherwise.
/// A number written `2.0` stays a `number`, because the integer refuses that spelling, and so does
/// one out of the integer's range.
pub(super) fn types_of(
    values: &[Value],
    declared: Option<&std::collections::BTreeSet<TypeName>>,
    unsigned: bool,
) -> std::collections::BTreeSet<TypeName> {
    let held = |value: &Value| {
        if unsigned {
            value.as_u64().is_some()
        } else {
            value.as_i64().is_some()
        }
    };
    let integer = declared.is_none_or(|declared| declared.contains(&TypeName::Integer))
        && values.iter().filter(|value| value.is_number()).all(held);
    values
        .iter()
        .filter_map(|value| match value {
            Value::Null => Some(TypeName::Null),
            Value::Bool(_) => Some(TypeName::Boolean),
            Value::String(_) => Some(TypeName::String),
            Value::Number(_) if integer => Some(TypeName::Integer),
            Value::Number(_) => Some(TypeName::Number),
            Value::Array(_) | Value::Object(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::same;

    /// Numbers compare by their decimal value, exactly, however they are spelled.
    #[test]
    fn numbers_are_the_same_by_value_where_that_can_be_told() {
        assert_eq!(same(&json!(2), &json!(2.0)), Some(true));
        assert_eq!(same(&json!(2), &json!(2.5)), Some(false));
        assert_eq!(same(&json!("2"), &json!(2)), Some(false));
        assert_eq!(
            same(
                &json!(9_007_199_254_740_993_u64),
                &json!(9_007_199_254_740_992_u64)
            ),
            Some(false)
        );
        // An integer above 2^53 and a float that reads as the same `f64` differ in their text.
        assert_eq!(
            same(
                &json!(9_007_199_254_740_993_u64),
                &json!(9_007_199_254_740_992.0)
            ),
            Some(false)
        );
        assert_eq!(same(&json!(-0.0), &json!(0)), Some(true));
        assert_eq!(same(&json!(1.50), &json!(1.5)), Some(true));
    }
}
