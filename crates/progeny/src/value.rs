//! Reading and writing JSON object nodes without losing members.
//!
//! The whole model is a superset of what progeny interprets: fields the pipeline understands
//! are typed, and every other member — `x-*` extensions, keywords from a future dialect, a
//! member whose value has the wrong shape — is preserved verbatim. That property is enforced
//! here by construction rather than by discipline: reading a node consumes members one at a
//! time and [`Members::rest`] hands back everything left over, so there is no code path that
//! can drop one. A member whose value does not typecheck is put *back* into the leftovers, so
//! even malformed input survives a round trip.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

use crate::diag::Ctx;

/// The members of one JSON object node, consumed as they are interpreted.
#[derive(Debug)]
pub(crate) struct Members {
    map: Map<String, Value>,
}

impl Members {
    pub(crate) fn new(map: Map<String, Value>) -> Self {
        Self { map }
    }

    /// Interpret `key`, or leave it among the leftovers.
    ///
    /// `convert` runs with the diagnostic location extended by `key`, and returns the value
    /// back through `Err` when it cannot interpret it. That is the single place where a
    /// mismatch is turned into both a preserved member and its diagnostic, so the two cannot
    /// come apart.
    pub(crate) fn typed<T>(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
        convert: impl FnOnce(Value, &mut Ctx) -> Result<T, Value>,
    ) -> Option<T> {
        let value = self.map.remove(key)?;
        match ctx.scoped(key, |ctx| convert(value, ctx)) {
            Ok(interpreted) => Some(interpreted),
            Err(original) => {
                self.map.insert(key.to_owned(), original);
                ctx.malformed(key, expected);
                None
            }
        }
    }

    /// Take `key` whatever it holds, including an explicit `null`.
    ///
    /// For members whose value is arbitrary JSON by definition — `default`, `const`,
    /// `example` — where there is nothing to typecheck and therefore nothing to salvage.
    pub(crate) fn value(&mut self, key: &str) -> Option<Value> {
        self.map.remove(key)
    }

    pub(crate) fn string(&mut self, key: &str, ctx: &mut Ctx) -> Option<String> {
        self.typed(key, ctx, "a string", |value, _| match value {
            Value::String(text) => Ok(text),
            other => Err(other),
        })
    }

    pub(crate) fn bool(&mut self, key: &str, ctx: &mut Ctx) -> Option<bool> {
        self.typed(key, ctx, "a boolean", |value, _| match value {
            Value::Bool(flag) => Ok(flag),
            other => Err(other),
        })
    }

    /// Take a numeric member as a [`Number`], keeping its literal form.
    ///
    /// Numbers are stored, not interpreted: `1` and `1.0` are different literals and stay
    /// different, and a bound wider than `u64` survives. Interpretation happens later,
    /// through accessors that can diagnose what they cannot use.
    pub(crate) fn number(&mut self, key: &str, ctx: &mut Ctx) -> Option<Number> {
        self.typed(key, ctx, "a number", |value, _| match value {
            Value::Number(number) => Ok(number),
            other => Err(other),
        })
    }

    pub(crate) fn string_array(&mut self, key: &str, ctx: &mut Ctx) -> Option<Vec<String>> {
        self.typed(key, ctx, "an array of strings", |value, _| match value {
            Value::Array(items) if items.iter().all(Value::is_string) => Ok(items
                .into_iter()
                .map(|item| match item {
                    Value::String(text) => text,
                    // Unreachable: the guard above checked every element.
                    other => other.to_string(),
                })
                .collect()),
            other => Err(other),
        })
    }

    pub(crate) fn value_array(&mut self, key: &str, ctx: &mut Ctx) -> Option<Vec<Value>> {
        self.typed(key, ctx, "an array", |value, _| match value {
            Value::Array(items) => Ok(items),
            other => Err(other),
        })
    }

    /// Read a nested object node.
    pub(crate) fn object<T>(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        read: impl FnOnce(Members, &mut Ctx) -> T,
    ) -> Option<T> {
        self.typed(key, ctx, "an object", |value, ctx| match value {
            Value::Object(map) => Ok(read(Members::new(map), ctx)),
            other => Err(other),
        })
    }

    /// Read an array of nodes, each of which must be an object.
    ///
    /// The whole array is salvaged when any element is not an object: an array of nodes with
    /// a hole in it has no faithful representation, and keeping the member verbatim tells the
    /// truth about what the document said.
    pub(crate) fn object_array<T>(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
        mut read: impl FnMut(Members, &mut Ctx) -> T,
    ) -> Option<Vec<T>> {
        self.typed(key, ctx, expected, |value, ctx| match value {
            Value::Array(items) if items.iter().all(Value::is_object) => Ok(items
                .into_iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    Value::Object(map) => {
                        Some(ctx.scoped(&index.to_string(), |ctx| read(Members::new(map), ctx)))
                    }
                    // Unreachable: the guard above checked every element.
                    _ => None,
                })
                .collect()),
            other => Err(other),
        })
    }

    /// Read an object whose members are themselves nodes, keyed by name.
    pub(crate) fn object_map<T>(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
        mut read: impl FnMut(Members, &mut Ctx) -> T,
    ) -> Option<BTreeMap<String, T>> {
        self.typed(key, ctx, expected, |value, ctx| match value {
            Value::Object(map) if map.values().all(Value::is_object) => Ok(map
                .into_iter()
                .filter_map(|(name, entry)| match entry {
                    Value::Object(entry) => {
                        let read = ctx.scoped(&name, |ctx| read(Members::new(entry), ctx));
                        Some((name, read))
                    }
                    // Unreachable: the guard above checked every member.
                    _ => None,
                })
                .collect()),
            other => Err(other),
        })
    }

    /// Read an object whose members are arbitrary JSON values, keyed by name.
    pub(crate) fn value_map(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
    ) -> Option<BTreeMap<String, Value>> {
        self.typed(key, ctx, expected, |value, _| match value {
            Value::Object(map) => Ok(map.into_iter().collect()),
            other => Err(other),
        })
    }

    /// Read an object whose members are strings, keyed by name.
    pub(crate) fn string_map(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
    ) -> Option<BTreeMap<String, String>> {
        self.typed(key, ctx, expected, |value, _| match value {
            Value::Object(map) if map.values().all(Value::is_string) => Ok(map
                .into_iter()
                .filter_map(|(name, entry)| match entry {
                    Value::String(text) => Some((name, text)),
                    // Unreachable: the guard above checked every member.
                    _ => None,
                })
                .collect()),
            other => Err(other),
        })
    }

    /// Read an object whose members are arrays of strings, keyed by name.
    pub(crate) fn string_array_map(
        &mut self,
        key: &str,
        ctx: &mut Ctx,
        expected: &str,
    ) -> Option<BTreeMap<String, Vec<String>>> {
        self.typed(key, ctx, expected, |value, _| match value {
            Value::Object(map)
                if map.values().all(|entry| {
                    entry
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
                }) =>
            {
                Ok(map
                    .into_iter()
                    .map(|(name, entry)| {
                        let scopes = entry
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                            .collect();
                        (name, scopes)
                    })
                    .collect())
            }
            other => Err(other),
        })
    }

    /// The names of the members not yet interpreted.
    ///
    /// Owned rather than borrowed so the caller can read the members it selects while iterating,
    /// which is what the open-map nodes (`paths`, `responses`, callbacks) need.
    pub(crate) fn keys(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub(crate) fn holds_string(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(Value::is_string)
    }

    pub(crate) fn holds_object(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(Value::is_object)
    }

    /// Every member that was never interpreted: `x-*` extensions, keywords progeny does not
    /// model, and members whose values did not typecheck.
    pub(crate) fn rest(self) -> BTreeMap<String, Value> {
        self.map.into_iter().collect()
    }
}

/// Builds one JSON object node back up from its typed fields plus its uninterpreted members.
#[cfg(any(feature = "harness", test))]
#[derive(Debug, Default)]
pub(crate) struct Builder {
    map: Map<String, Value>,
}

#[cfg(any(feature = "harness", test))]
impl Builder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Write `key` when the field is present, and write nothing when it is absent.
    ///
    /// Absent and present-but-empty are different documents (`security: []` disables security
    /// where an absent `security` inherits it), so every optional field is an `Option` of its
    /// collection rather than an always-present empty one.
    pub(crate) fn set(&mut self, key: &str, value: Option<impl Into<Value>>) {
        if let Some(value) = value {
            self.map.insert(key.to_owned(), value.into());
        }
    }

    /// Write `key` from a field that needs conversion, when present.
    pub(crate) fn set_with<T>(
        &mut self,
        key: &str,
        value: Option<T>,
        into: impl FnOnce(T) -> Value,
    ) {
        if let Some(value) = value {
            self.map.insert(key.to_owned(), into(value));
        }
    }

    /// Write a whole array from nodes that each need conversion.
    pub(crate) fn set_array<T>(
        &mut self,
        key: &str,
        values: Option<&[T]>,
        mut into: impl FnMut(&T) -> Value,
    ) {
        if let Some(values) = values {
            let items = values.iter().map(&mut into).collect();
            self.map.insert(key.to_owned(), Value::Array(items));
        }
    }

    /// Write a whole object from nodes that each need conversion.
    pub(crate) fn set_map<T>(
        &mut self,
        key: &str,
        values: Option<&BTreeMap<String, T>>,
        mut into: impl FnMut(&T) -> Value,
    ) {
        if let Some(values) = values {
            let members = values
                .iter()
                .map(|(name, entry)| (name.clone(), into(entry)))
                .collect();
            self.map.insert(key.to_owned(), Value::Object(members));
        }
    }

    /// Write back the members that were never interpreted.
    pub(crate) fn extend(&mut self, members: &BTreeMap<String, Value>) {
        for (key, value) in members {
            self.map.insert(key.clone(), value.clone());
        }
    }

    pub(crate) fn finish(self) -> Value {
        Value::Object(self.map)
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{Builder, Members};
    use crate::diag::Ctx;

    fn members(value: Value) -> Members {
        match value {
            Value::Object(map) => Members::new(map),
            other => panic!("not an object: {other}"),
        }
    }

    #[test_util::test]
    fn uninterpreted_members_survive() {
        let mut ctx = Ctx::new();
        let mut node = members(json!({
            "title": "Petstore",
            "x-vendor": {"nested": [1, 2]},
            "unknownKeyword": true,
        }));

        assert_eq!(node.string("title", &mut ctx).as_deref(), Some("Petstore"));
        let rest = node.rest();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest["x-vendor"], json!({"nested": [1, 2]}));
        assert_eq!(rest["unknownKeyword"], json!(true));
        assert!(ctx.into_diagnostics().is_empty());
    }

    #[test_util::test]
    fn a_mismatched_member_is_put_back_and_diagnosed() {
        let mut ctx = Ctx::new();
        let mut node = members(json!({"title": 42}));

        assert_eq!(node.string("title", &mut ctx), None);
        assert_eq!(node.rest()["title"], json!(42));

        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location().to_string(), "/title");
        assert!(diagnostics[0].detail().contains("a string"));
    }

    #[test_util::test]
    fn an_explicit_null_is_not_the_same_as_an_absent_member() {
        let ctx = Ctx::new();
        let mut node = members(json!({"default": null}));
        assert_eq!(node.value("default"), Some(Value::Null));
        assert_eq!(node.value("default"), None);
        assert!(ctx.into_diagnostics().is_empty());
    }

    #[test_util::test]
    fn an_array_with_one_bad_element_is_salvaged_whole() {
        let mut ctx = Ctx::new();
        let mut node = members(json!({"servers": [{"url": "/a"}, "nope"]}));

        let read = node.object_array(
            "servers",
            &mut ctx,
            "an array of servers",
            |mut server, ctx| server.string("url", ctx),
        );
        assert_eq!(read, None);
        assert_eq!(node.rest()["servers"], json!([{"url": "/a"}, "nope"]));
        assert_eq!(ctx.into_diagnostics().len(), 1);
    }

    #[test_util::test]
    fn nested_diagnostics_carry_the_full_path() {
        let mut ctx = Ctx::new();
        let mut node = members(json!({"servers": [{"url": 7}]}));

        node.object_array(
            "servers",
            &mut ctx,
            "an array of servers",
            |mut server, ctx| {
                server.string("url", ctx);
                server.rest()
            },
        );
        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location().to_string(), "/servers/0/url");
    }

    #[test_util::test]
    fn numbers_keep_their_literal_form() {
        let mut ctx = Ctx::new();
        let value: Value = serde_json::from_str(r#"{"a": 1.0, "b": 1}"#)?;
        let mut node = members(value);
        let a = node
            .number("a", &mut ctx)
            .ok_or_eyre("test fixture should contain this value")?;
        let b = node
            .number("b", &mut ctx)
            .ok_or_eyre("test fixture should contain this value")?;
        assert_eq!(a.to_string(), "1.0");
        assert_eq!(b.to_string(), "1");
        assert_ne!(a, b);
    }

    #[test_util::test]
    fn builder_omits_absent_fields_and_writes_empty_ones() {
        let mut builder = Builder::new();
        builder.set("present", Some("x"));
        builder.set("absent", None::<String>);
        builder.set_array("empty", Some(&[] as &[String]), |item| json!(item));
        assert_eq!(builder.finish(), json!({"present": "x", "empty": []}));
    }
}
