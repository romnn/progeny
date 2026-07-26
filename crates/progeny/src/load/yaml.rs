//! YAML to `serde_json::Value`, with YAML 1.2 core-schema semantics.
//!
//! Built on the event stream rather than on a YAML value type, because three of the decisions
//! this loader has to make are not expressible after the fact:
//!
//! * **Which scalars are numbers.** YAML 1.2's core schema resolves `y`, `n`, `on`, `off` and
//!   `=` as strings. YAML 1.1 resolves the first four as booleans and `=` as a special tag,
//!   which would silently corrupt real documents: one corpus spec requires a property named
//!   `y` and another a JWK field named `n`, so a 1.1 loader turns `required: [x, y]` into
//!   `required: [x, true]`.
//! * **What a number's literal was.** `1` and `1.0` are different defaults to render into
//!   generated code, so the raw scalar text is preserved instead of being routed through
//!   `f64`.
//! * **Which keys are strings.** YAML permits non-string mapping keys and real documents rely
//!   on it constantly — `200:` under `responses` is an integer key. Scalar keys are
//!   stringified with their canonical JSON rendering, silently, because the OpenAPI meaning is
//!   unambiguous. Non-scalar keys have no unambiguous member name and are rejected.
//!
//! Anchors, aliases and merge keys are resolved here: they are a serialization artifact rather
//! than document semantics, so nothing downstream needs to know they were used.
//!
//! One repair happens at the text level, before the parser sees anything: a document whose last
//! line has no line break gets one. Roughly half the corpus is served that way, and without the
//! break the parser cannot finish reading a block scalar that runs to the end of the file. The
//! only observable effect is on such a scalar — clip chomping then keeps a newline the document
//! did not write — so that, and only that, is diagnosed.
//!
//! The parse also runs inside a panic boundary. The generator must not panic on any input, because
//! a panic is a rejection with no diagnostic; the underlying parser is a port of a C library and
//! does panic on a few adversarial inputs (an unterminated flow mapping whose first token is a tag
//! indicator, for one). The boundary turns that into an ordinary rejection, which is what the
//! invariant is actually asking for. It is the only such boundary in the crate, and it is the
//! reason a fuzz target exists from the first stage rather than the last.

use std::collections::BTreeMap;

use libyaml_safer::{EventData, ScalarStyle};
use serde_json::{Map, Number, Value};

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};

/// How deeply a document may nest.
///
/// This is not a taste limit, it is a safety one: `serde_json::Value` is a recursive type, so
/// building, walking and *dropping* one recurses, and a document nested ten thousand deep would
/// overflow the stack — an abort, which is strictly worse than a rejection because it carries no
/// diagnostic. `serde_json` imposes the same limit on the JSON side by default, so this keeps the
/// two formats answering alike. Real documents nest an order of magnitude less than this.
const MAX_DEPTH: usize = 128;

/// The keywords where a number is an assertion about instances rather than an annotation.
///
/// A non-representable value in one of these means the constraint is ignored — the generated type
/// accepts more than the document said, which is a degradation. Anywhere else the value itself is
/// dropped, which is a repair: an annotation carries no assertion.
const VALIDATION_KEYWORDS: &[&str] = &[
    "exclusiveMaximum",
    "exclusiveMinimum",
    "maximum",
    "minimum",
    "multipleOf",
];

/// Load the first document of a YAML stream.
pub(crate) fn load(text: &str, ctx: &mut Ctx) -> Result<Value, RejectError> {
    // Copy only when the break is actually missing; a document that ends properly is parsed in
    // place.
    let supplied_break = !text.ends_with('\n');
    let owned = if supplied_break {
        Some(format!("{text}\n"))
    } else {
        None
    };
    let text = owned.as_deref().unwrap_or(text);

    let loader = Loader {
        ctx,
        frames: Vec::new(),
        anchors: BTreeMap::new(),
        root: None,
        supplied_break,
        last_scalar_was_block: false,
    };

    // `AssertUnwindSafe` is sound here because nothing observable survives a panic: the loader and
    // everything it built are dropped, and the diagnostics collected so far are discarded with the
    // rejection. A caller that builds with `panic = "abort"` gets an abort instead, which no
    // library can intercept.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loader.run(text))) {
        Ok(result) => result,
        Err(_) => Err(RejectError::new(
            RejectKind::Unparsable,
            "the YAML parser gave up part-way through this document",
        )),
    }
}

/// A collection whose child nodes are still arriving.
#[derive(Debug)]
enum Frame {
    Sequence {
        anchor: Option<String>,
        items: Vec<Value>,
    },
    Mapping {
        anchor: Option<String>,
        entries: Vec<(String, Value)>,
        merges: Vec<Value>,
        /// The key whose value is currently being built; `None` while a key is being read.
        pending_key: Option<String>,
        /// Whether `pending_key` is the merge key, so its value is a source of defaults
        /// rather than a member.
        pending_is_merge: bool,
    },
}

struct Loader<'a> {
    ctx: &'a mut Ctx,
    frames: Vec<Frame>,
    /// Completed nodes by anchor name, for aliases to copy.
    anchors: BTreeMap<String, Value>,
    root: Option<Value>,
    /// Whether the final line break had to be supplied.
    supplied_break: bool,
    /// Whether the last scalar read was a block scalar, which is the only kind whose value a
    /// supplied final break can change.
    last_scalar_was_block: bool,
}

impl Loader<'_> {
    fn run(mut self, text: &str) -> Result<Value, RejectError> {
        let mut input = text.as_bytes();
        let mut parser = libyaml_safer::Parser::new();
        parser.set_input(&mut input);
        loop {
            let event = parser.parse().map_err(|error| {
                RejectError::new(RejectKind::Unparsable, error.to_string()).at(self.location())
            })?;
            if self.handle(event.data)? {
                break;
            }
        }
        if self.supplied_break && self.last_scalar_was_block {
            self.ctx.report(Diagnostic::new(
                BreakageClass::MissingFinalLineBreak,
                Action::Repair,
                JsonPointer::root(),
                "the document's last line has no line break and ends inside a block scalar; \
                 supplied the break, so that scalar keeps one trailing newline more than the \
                 document wrote",
            ));
        }

        // An empty stream is an empty document, which the next stage rejects for having no
        // version rather than being rejected here for being unparsable — the document parsed
        // fine, it just says nothing.
        Ok(self.root.take().unwrap_or(Value::Null))
    }

    /// Consume one event. Returns whether the loader is done.
    fn handle(&mut self, event: EventData) -> Result<bool, RejectError> {
        match event {
            EventData::StreamStart { .. } | EventData::DocumentStart { .. } => {}
            // Everything after the first document is another document; an OpenAPI description is
            // one document, so stop at the end of the first.
            EventData::DocumentEnd { .. } | EventData::StreamEnd => return Ok(true),
            EventData::Scalar {
                anchor,
                tag,
                value,
                style,
                ..
            } => self.scalar(&value, style, anchor, tag.as_deref()),
            EventData::Alias { anchor } => self.alias(&anchor)?,
            EventData::SequenceStart { anchor, .. } => {
                self.reject_collection_key("a sequence")?;
                self.check_depth()?;
                self.frames.push(Frame::Sequence {
                    anchor,
                    items: Vec::new(),
                });
            }
            EventData::MappingStart { anchor, .. } => {
                self.reject_collection_key("a mapping")?;
                self.check_depth()?;
                self.frames.push(Frame::Mapping {
                    anchor,
                    entries: Vec::new(),
                    merges: Vec::new(),
                    pending_key: None,
                    pending_is_merge: false,
                });
            }
            EventData::SequenceEnd | EventData::MappingEnd => {
                let (anchor, value) = self.close_frame();
                self.remember(anchor, &value);
                self.place(value);
            }
        }
        Ok(false)
    }

    fn scalar(
        &mut self,
        text: &str,
        style: ScalarStyle,
        anchor: Option<String>,
        tag: Option<&str>,
    ) {
        self.last_scalar_was_block = matches!(style, ScalarStyle::Literal | ScalarStyle::Folded);
        match resolve(text, style, tag) {
            Resolved::Value(value) => {
                self.remember(anchor, &value);
                if self.expecting_key() {
                    self.begin_key(key_name(&value, text), text, style, tag);
                } else {
                    self.place(value);
                }
            }
            Resolved::NonFinite => {
                if self.expecting_key() {
                    // A key is a member name, not a quantity; the literal text is its name and
                    // nothing about it is unrepresentable.
                    self.begin_key(text.to_owned(), text, style, tag);
                } else {
                    self.drop_non_finite(text);
                }
            }
        }
    }

    fn alias(&mut self, anchor: &str) -> Result<(), RejectError> {
        let Some(value) = self.anchors.get(anchor).cloned() else {
            return Err(RejectError::new(
                RejectKind::Unparsable,
                format!("`*{anchor}` aliases an anchor that was never defined"),
            )
            .at(self.location()));
        };
        if self.expecting_key() {
            if value.is_array() || value.is_object() {
                return Err(RejectError::new(
                    RejectKind::NonScalarKey,
                    "a mapping key aliases a collection, so it has no member name",
                )
                .at(self.location()));
            }
            let name = key_name(&value, "");
            self.begin_key(name, "", ScalarStyle::Plain, None);
        } else {
            self.place(value);
        }
        Ok(())
    }

    /// Whether the next node read will be a mapping key.
    fn expecting_key(&self) -> bool {
        matches!(
            self.frames.last(),
            Some(Frame::Mapping {
                pending_key: None,
                ..
            })
        )
    }

    fn begin_key(&mut self, name: String, raw: &str, style: ScalarStyle, tag: Option<&str>) {
        // The merge key is only the merge key when written plainly and untagged; `"<<"` is an
        // ordinary member name.
        let is_merge = raw == "<<" && style == ScalarStyle::Plain && tag.is_none();
        if let Some(Frame::Mapping {
            pending_key,
            pending_is_merge,
            ..
        }) = self.frames.last_mut()
        {
            *pending_key = Some(name);
            *pending_is_merge = is_merge;
        }
    }

    fn check_depth(&self) -> Result<(), RejectError> {
        if self.frames.len() >= MAX_DEPTH {
            return Err(RejectError::new(
                RejectKind::Unparsable,
                format!("the document nests more than {MAX_DEPTH} levels deep"),
            )
            .at(self.location()));
        }
        Ok(())
    }

    fn reject_collection_key(&self, what: &str) -> Result<(), RejectError> {
        if self.expecting_key() {
            return Err(RejectError::new(
                RejectKind::NonScalarKey,
                format!("a mapping key is {what}, so it has no member name"),
            )
            .at(self.location()));
        }
        Ok(())
    }

    /// Hand a completed node to whatever is waiting for it.
    fn place(&mut self, value: Value) {
        match self.frames.last_mut() {
            None => self.root = Some(value),
            Some(Frame::Sequence { items, .. }) => items.push(value),
            Some(Frame::Mapping {
                entries,
                merges,
                pending_key,
                pending_is_merge,
                ..
            }) => {
                if let Some(key) = pending_key.take() {
                    if std::mem::take(pending_is_merge) {
                        merges.push(value);
                    } else {
                        entries.push((key, value));
                    }
                }
            }
        }
    }

    fn remember(&mut self, anchor: Option<String>, value: &Value) {
        if let Some(anchor) = anchor {
            self.anchors.insert(anchor, value.clone());
        }
    }

    /// Finish the innermost collection.
    fn close_frame(&mut self) -> (Option<String>, Value) {
        match self.frames.pop() {
            Some(Frame::Sequence { anchor, items }) => (anchor, Value::Array(items)),
            Some(Frame::Mapping {
                anchor,
                entries,
                merges,
                ..
            }) => (anchor, self.build_mapping(entries, merges)),
            // Unreachable: the parser only emits an end event for a collection it started.
            None => (None, Value::Null),
        }
    }

    fn build_mapping(&mut self, entries: Vec<(String, Value)>, merges: Vec<Value>) -> Value {
        let mut map = Map::new();
        // Duplicate keys keep the last value, matching what every JSON parser does with them
        // and therefore what every other generator sees.
        for (key, value) in entries {
            map.insert(key, value);
        }
        for source in merges {
            self.merge_into(&mut map, source);
        }
        Value::Object(map)
    }

    /// Apply one merge source: explicit members always win, and among several sources the
    /// earlier one wins.
    fn merge_into(&mut self, map: &mut Map<String, Value>, source: Value) {
        match source {
            Value::Object(members) => {
                for (key, value) in members {
                    map.entry(key).or_insert(value);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.merge_into(map, item);
                }
            }
            other => {
                let location = self.location();
                self.ctx.report(Diagnostic::new(
                    BreakageClass::MalformedMember,
                    Action::Degrade,
                    location.child("<<"),
                    "the merge key's value is neither a mapping nor a sequence of mappings; \
                     kept it verbatim as a member named `<<`",
                ));
                map.entry("<<".to_owned()).or_insert(other);
            }
        }
    }

    /// Drop a value that JSON cannot hold, and say so.
    fn drop_non_finite(&mut self, literal: &str) {
        let keyword = self.enclosing_key().map(ToOwned::to_owned);
        let validation = keyword
            .as_deref()
            .is_some_and(|key| VALIDATION_KEYWORDS.contains(&key));
        let location = self.location();
        let (action, detail) = if validation {
            (
                Action::Degrade,
                format!(
                    "`{literal}` is not representable in JSON; ignored the constraint, so the \
                     generated type accepts values the document excludes"
                ),
            )
        } else {
            (
                Action::Repair,
                format!("`{literal}` is not representable in JSON; dropped the value"),
            )
        };
        self.ctx.report(Diagnostic::new(
            BreakageClass::NonFiniteNumber,
            action,
            location,
            detail,
        ));

        // Dropping means never placing: a mapping loses the whole member, a sequence loses the
        // element, and a lone scalar document becomes empty.
        match self.frames.last_mut() {
            None => self.root = Some(Value::Null),
            Some(Frame::Sequence { .. }) => {}
            Some(Frame::Mapping {
                pending_key,
                pending_is_merge,
                ..
            }) => {
                *pending_key = None;
                *pending_is_merge = false;
            }
        }
    }

    /// Where the node currently being read lives.
    fn location(&self) -> JsonPointer {
        let mut pointer = JsonPointer::root();
        for frame in &self.frames {
            match frame {
                Frame::Sequence { items, .. } => pointer.push(items.len().to_string()),
                Frame::Mapping {
                    pending_key: Some(key),
                    ..
                } => pointer.push(key.clone()),
                Frame::Mapping {
                    pending_key: None, ..
                } => {}
            }
        }
        pointer
    }

    /// The nearest mapping key above the node currently being read — the keyword whose value
    /// this is, however deeply nested.
    fn enclosing_key(&self) -> Option<&str> {
        self.frames.iter().rev().find_map(|frame| match frame {
            Frame::Mapping {
                pending_key: Some(key),
                ..
            } => Some(key.as_str()),
            _ => None,
        })
    }
}

/// A scalar's canonical JSON rendering as a member name.
fn key_name(value: &Value, raw: &str) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        // Collections never reach here; callers reject them first.
        Value::Array(_) | Value::Object(_) => raw.to_owned(),
    }
}

enum Resolved {
    Value(Value),
    /// A number JSON cannot hold: `.inf`, `-.inf`, `.nan`.
    NonFinite,
}

/// The tag prefix the YAML core schema's own tags share.
const CORE_SCHEMA: &str = "tag:yaml.org,2002:";

/// Resolve one scalar to a JSON value, per the YAML 1.2 core schema.
fn resolve(text: &str, style: ScalarStyle, tag: Option<&str>) -> Resolved {
    if let Some(tag) = tag {
        let Some(suffix) = tag.strip_prefix(CORE_SCHEMA) else {
            // An application-specific tag says nothing progeny can act on; the text is the
            // only thing it can hold onto.
            return Resolved::Value(Value::String(text.to_owned()));
        };
        return match suffix {
            "null" => Resolved::Value(Value::Null),
            "bool" => Resolved::Value(
                parse_bool(text).map_or_else(|| Value::String(text.to_owned()), Value::Bool),
            ),
            "int" | "float" if non_finite(text) => Resolved::NonFinite,
            "int" | "float" => Resolved::Value(
                parse_number(text).map_or_else(|| Value::String(text.to_owned()), Value::Number),
            ),
            // `!!str`, `!!merge`, `!!value` and anything else: the text, verbatim.
            _ => Resolved::Value(Value::String(text.to_owned())),
        };
    }

    // Quoted, literal and folded scalars are strings by construction; only plain scalars are
    // resolved by their spelling.
    if style != ScalarStyle::Plain {
        return Resolved::Value(Value::String(text.to_owned()));
    }

    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Resolved::Value(Value::Null),
        "true" | "True" | "TRUE" => return Resolved::Value(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Resolved::Value(Value::Bool(false)),
        _ => {}
    }
    if non_finite(text) {
        return Resolved::NonFinite;
    }
    Resolved::Value(
        parse_number(text).map_or_else(|| Value::String(text.to_owned()), Value::Number),
    )
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn non_finite(text: &str) -> bool {
    let magnitude = text.strip_prefix(['+', '-']).unwrap_or(text);
    matches!(magnitude, ".inf" | ".Inf" | ".INF") || matches!(text, ".nan" | ".NaN" | ".NAN")
}

/// Parse a YAML core-schema number, keeping its literal form wherever JSON can express it.
fn parse_number(text: &str) -> Option<Number> {
    let canonical = canonical_json_number(text)?;
    match serde_json::from_str::<Value>(&canonical) {
        Ok(Value::Number(number)) => Some(number),
        _ => None,
    }
}

/// Rewrite a YAML number literal as the JSON literal for the same value.
///
/// For the overwhelming majority of literals this is the identity, which is the point: JSON's
/// grammar is a subset of YAML's, so `1.0` stays the four characters `1.0` and a 40-digit
/// decimal keeps all forty digits. Only the spellings JSON has no syntax for — a leading `+`,
/// a bare leading `.`, a trailing `.`, leading zeros, hexadecimal and octal — are rewritten.
fn canonical_json_number(text: &str) -> Option<String> {
    let (sign, magnitude) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };
    if magnitude.is_empty() {
        return None;
    }

    if let Some(hex) = strip_prefix_either(magnitude, "0x", "0X") {
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = i128::from_str_radix(hex, 16).ok()?;
        return Some(format!("{sign}{value}"));
    }
    if let Some(octal) = strip_prefix_either(magnitude, "0o", "0O") {
        if octal.is_empty()
            || !octal
                .bytes()
                .all(|byte| byte.is_ascii_digit() && byte < b'8')
        {
            return None;
        }
        let value = i128::from_str_radix(octal, 8).ok()?;
        return Some(format!("{sign}{value}"));
    }

    let (mantissa, exponent) = match magnitude.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (magnitude, None),
    };
    let (integer, fraction) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (mantissa, None),
    };

    if !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if let Some(fraction) = fraction
        && !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // `.` and `.e3` are not numbers; there has to be a digit somewhere.
    if integer.is_empty() && fraction.is_none_or(str::is_empty) {
        return None;
    }

    let mut out = String::with_capacity(text.len() + 2);
    out.push_str(sign);
    out.push_str(&trim_leading_zeros(integer));
    if let Some(fraction) = fraction {
        out.push('.');
        // JSON needs a digit after the point; YAML's `1.` means one.
        out.push_str(if fraction.is_empty() { "0" } else { fraction });
    }
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        out.push('e');
        out.push_str(exponent);
    }
    Some(out)
}

fn strip_prefix_either<'a>(text: &'a str, one: &str, other: &str) -> Option<&'a str> {
    text.strip_prefix(one).or_else(|| text.strip_prefix(other))
}

/// JSON forbids leading zeros; YAML reads them as decimal.
fn trim_leading_zeros(digits: &str) -> String {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{canonical_json_number, load};
    use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, RejectKind};

    fn yaml(text: &str) -> Value {
        let mut ctx = Ctx::new();
        load(text, &mut ctx).unwrap()
    }

    fn yaml_with_diagnostics(text: &str) -> (Value, Vec<Diagnostic>) {
        let mut ctx = Ctx::new();
        let value = load(text, &mut ctx).unwrap();
        (value, ctx.into_diagnostics())
    }

    #[test]
    fn integer_keys_become_their_canonical_string() {
        assert_eq!(
            yaml("responses:\n  200:\n    description: ok\n  \"404\":\n    description: no\n"),
            json!({"responses": {"200": {"description": "ok"}, "404": {"description": "no"}}})
        );
    }

    #[test]
    fn every_scalar_kind_of_key_gets_a_canonical_name() {
        // `null` and `~` are the same key, so the later one wins, exactly as two identical keys
        // would. `0200` is the integer 200, and its canonical JSON rendering has no leading zero.
        assert_eq!(
            yaml("true: a\nnull: b\n~: c\n0200: d\n1.50: e\n"),
            json!({"true": "a", "null": "c", "200": "d", "1.50": "e"})
        );
    }

    #[test]
    fn core_schema_does_not_resolve_yaml_11_booleans() {
        // `figma` requires a property named `y`; `stytch` a JWK field named `n`. A YAML 1.1
        // loader turns these into booleans and corrupts the model.
        assert_eq!(
            yaml("required:\n  - x\n  - y\n  - n\n  - on\n  - off\n  - yes\n  - no\n"),
            json!({"required": ["x", "y", "n", "on", "off", "yes", "no"]})
        );
    }

    #[test]
    fn a_bare_equals_sign_is_a_string() {
        // `zendesk` has `change: =` inside an example. YAML 1.1 resolves `=` to the `!!value`
        // tag, which loaders without a constructor for it reject outright.
        assert_eq!(yaml("change: =\n"), json!({"change": "="}));
    }

    #[test]
    fn number_literals_survive_exactly() {
        // Every digit is kept: `1.0` does not collapse to `1`, `1.00` does not collapse to `1.0`,
        // and a float that only `f64` round-tripping would mangle keeps all seventeen digits. The
        // one thing that is normalized is the exponent's sign, which the value type writes
        // explicitly; it is the same number either way and it is deterministic.
        let value = yaml("a: 1\nb: 1.0\nc: 1.00\nd: 1e3\ne: 0.30000000000000004\n");
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(
            text,
            r#"{"a":1,"b":1.0,"c":1.00,"d":1e+3,"e":0.30000000000000004}"#
        );
    }

    #[test]
    fn numbers_json_cannot_spell_are_rewritten_to_the_same_value() {
        assert_eq!(canonical_json_number("+5").as_deref(), Some("5"));
        assert_eq!(canonical_json_number("-5").as_deref(), Some("-5"));
        assert_eq!(canonical_json_number("007").as_deref(), Some("7"));
        assert_eq!(canonical_json_number("-007").as_deref(), Some("-7"));
        assert_eq!(canonical_json_number("0").as_deref(), Some("0"));
        assert_eq!(canonical_json_number("00").as_deref(), Some("0"));
        assert_eq!(canonical_json_number(".5").as_deref(), Some("0.5"));
        assert_eq!(canonical_json_number("5.").as_deref(), Some("5.0"));
        assert_eq!(canonical_json_number("0x1F").as_deref(), Some("31"));
        assert_eq!(canonical_json_number("-0x10").as_deref(), Some("-16"));
        assert_eq!(canonical_json_number("0o17").as_deref(), Some("15"));
        assert_eq!(canonical_json_number("1.5E+3").as_deref(), Some("1.5e+3"));
        // Identity for everything JSON can already spell.
        for literal in ["1", "1.0", "-2.75", "1e3", "12345678901234567890.5"] {
            assert_eq!(canonical_json_number(literal).as_deref(), Some(literal));
        }
    }

    #[test]
    fn scalars_that_only_look_numeric_stay_strings() {
        for text in [
            ".", "-", "12:30", "1_000", "0x", "0o8", "1e", "1.2.3", "abc",
        ] {
            assert_eq!(canonical_json_number(text), None, "{text}");
        }
        assert_eq!(yaml("at: 12:30\n"), json!({"at": "12:30"}));
    }

    #[test]
    fn quoted_scalars_are_never_resolved() {
        assert_eq!(
            yaml("a: \"1\"\nb: 'true'\nc: \"null\"\n"),
            json!({"a": "1", "b": "true", "c": "null"})
        );
    }

    #[test]
    fn block_scalars_keep_their_text() {
        assert_eq!(
            yaml("a: |\n  one\n  two\nb: >-\n  folded\n  text\n"),
            json!({"a": "one\ntwo\n", "b": "folded text"})
        );
    }

    #[test]
    fn aliases_are_resolved_by_copying() {
        // `workos` and `openai` both use anchors, including on collections.
        let value = yaml(concat!(
            "shared: &group\n",
            "  - a\n",
            "  - b\n",
            "first: *group\n",
            "second: *group\n",
        ));
        assert_eq!(
            value,
            json!({"shared": ["a", "b"], "first": ["a", "b"], "second": ["a", "b"]})
        );
    }

    #[test]
    fn an_anchor_on_a_scalar_is_usable_as_a_key_and_a_value() {
        assert_eq!(
            yaml("name: &n title\nalias: *n\n"),
            json!({"name": "title", "alias": "title"})
        );
    }

    #[test]
    fn merge_keys_are_resolved_with_explicit_members_winning() {
        let value = yaml(concat!(
            "base: &base\n",
            "  type: string\n",
            "  description: from base\n",
            "derived:\n",
            "  <<: *base\n",
            "  description: mine\n",
        ));
        assert_eq!(
            value["derived"],
            json!({"type": "string", "description": "mine"})
        );
    }

    #[test]
    fn a_sequence_of_merge_sources_prefers_the_earlier_one() {
        let value = yaml(concat!(
            "a: &a\n",
            "  k: from a\n",
            "b: &b\n",
            "  k: from b\n",
            "  extra: yes\n",
            "merged:\n",
            "  <<: [*a, *b]\n",
        ));
        assert_eq!(value["merged"], json!({"k": "from a", "extra": "yes"}));
    }

    #[test]
    fn a_quoted_merge_key_is_an_ordinary_member() {
        assert_eq!(yaml("a:\n  \"<<\": x\n"), json!({"a": {"<<": "x"}}));
    }

    #[test]
    fn a_non_scalar_key_is_rejected() {
        let mut ctx = Ctx::new();
        let error = load("? [a, b]\n: value\n", &mut ctx).unwrap_err();
        assert_eq!(error.kind(), RejectKind::NonScalarKey);
    }

    #[test]
    fn a_non_finite_annotation_drops_the_value() {
        let (value, diagnostics) =
            yaml_with_diagnostics("schema:\n  default: .inf\n  type: number\n");
        assert_eq!(value, json!({"schema": {"type": "number"}}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::NonFiniteNumber);
        assert_eq!(diagnostics[0].action(), Action::Repair);
        assert_eq!(diagnostics[0].location().to_string(), "/schema/default");
    }

    #[test]
    fn a_non_finite_constraint_degrades_the_type() {
        let (value, diagnostics) = yaml_with_diagnostics("maximum: -.inf\nminimum: 0\n");
        assert_eq!(value, json!({"minimum": 0}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].action(), Action::Degrade);
        assert_eq!(diagnostics[0].location().to_string(), "/maximum");
    }

    #[test]
    fn a_non_finite_sequence_element_is_dropped_with_its_position() {
        let (value, diagnostics) = yaml_with_diagnostics("enum:\n  - 1\n  - .nan\n  - 3\n");
        assert_eq!(value, json!({"enum": [1, 3]}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location().to_string(), "/enum/1");
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        assert_eq!(yaml("a: 1\na: 2\n"), json!({"a": 2}));
    }

    #[test]
    fn an_empty_document_loads_as_null() {
        assert_eq!(yaml(""), Value::Null);
        assert_eq!(yaml("# just a comment\n"), Value::Null);
    }

    #[test]
    fn only_the_first_document_of_a_stream_is_loaded() {
        assert_eq!(yaml("a: 1\n---\nb: 2\n"), json!({"a": 1}));
    }

    #[test]
    fn a_dangling_alias_is_a_rejection_rather_than_a_panic() {
        let mut ctx = Ctx::new();
        assert!(load("a: *nope\n", &mut ctx).is_err());
    }

    #[test]
    fn a_parser_panic_becomes_an_ordinary_rejection() {
        // An unterminated flow mapping whose first token is a tag indicator makes the underlying
        // parser give up by panicking. The invariant is that no input panics *progeny*, so the
        // boundary turns it into a rejection. Keep the hook quiet so the test output stays
        // readable; the hook is process-global, so this is a test-only liberty.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut ctx = Ctx::new();
        let outcome = load("{!0,5',:c..8-\n", &mut ctx);
        std::panic::set_hook(previous);

        let error = outcome.unwrap_err();
        assert_eq!(error.kind(), RejectKind::Unparsable);
    }

    #[test]
    fn flow_style_is_the_same_document_as_block_style() {
        assert_eq!(
            yaml("{a: [1, 2], b: {c: d}}"),
            yaml("a:\n  - 1\n  - 2\nb:\n  c: d\n")
        );
    }

    #[test]
    fn explicit_core_schema_tags_are_honoured() {
        assert_eq!(
            yaml("a: !!str 1\nb: !!int \"2\"\nc: !!bool \"true\"\nd: !!null \"\"\n"),
            json!({"a": "1", "b": 2, "c": true, "d": null})
        );
    }
}
