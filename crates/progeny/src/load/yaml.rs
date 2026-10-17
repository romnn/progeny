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
//!   generated code, so the raw scalar text is kept with the `exact-numbers` feature instead of
//!   being routed through `f64`.
//!   Without it, a number the model holds only roughly is reported, and one beyond an `f64` is
//!   dropped and reported.
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

use super::number::{self, Held};
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
        /// How many elements were dropped, so a location names the element where the document
        /// has it rather than where the kept ones put it.
        dropped: usize,
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
    anchors: BTreeMap<String, Anchored>,
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
                    dropped: 0,
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
        // What the scalar is, and so what an alias of it is again.
        let anchored = match resolve(text, style, tag) {
            Resolved::Value(value) => Anchored::Node {
                key: Some(key_name(&value, text)),
                value,
            },
            Resolved::Rounded(number) => {
                let value = Value::Number(number);
                Anchored::Rounded {
                    key: key_name(&value, text),
                    literal: text.to_owned(),
                    value,
                }
            }
            Resolved::NonFinite => Anchored::Dropped {
                key: text.to_owned(),
                literal: text.to_owned(),
                why: NOT_JSON,
            },
            // Named the way it is when the model holds it, so the key is the same either way.
            Resolved::BeyondRange => Anchored::Dropped {
                key: canonical_json_number(text).map_or_else(
                    || text.to_owned(),
                    |canonical| number::key_spelling(&canonical),
                ),
                literal: text.to_owned(),
                why: BEYOND_RANGE,
            },
        };
        if let Some(anchor) = anchor {
            self.anchors.insert(anchor, anchored.clone());
        }
        if self.expecting_key() {
            // A key is a member name, not a quantity, so nothing about it is rounded or
            // unrepresentable: it is named after what the document wrote.
            let name = anchored.key().unwrap_or_else(|| text.to_owned());
            self.begin_key(name, text, style, tag);
        } else {
            self.place_anchored(anchored);
        }
    }

    /// Place a scalar's value, or an alias's, reporting a rounded number and dropping one the
    /// model cannot hold wherever it lands.
    fn place_anchored(&mut self, anchored: Anchored) {
        match anchored {
            Anchored::Node { value, .. } => self.place(value),
            Anchored::Rounded { value, literal, .. } => {
                let location = self.location();
                self.ctx.report(number::rounded(location, &literal, &value));
                self.place(value);
            }
            Anchored::Dropped { literal, why, .. } => self.drop_unrepresentable(&literal, why),
        }
    }

    fn alias(&mut self, anchor: &str) -> Result<(), RejectError> {
        let Some(anchored) = self.anchors.get(anchor).cloned() else {
            return Err(RejectError::new(
                RejectKind::Unparsable,
                format!("`*{anchor}` aliases an anchor that was never defined"),
            )
            .at(self.location()));
        };
        if self.expecting_key() {
            // Only a scalar has a member name.
            let Some(name) = anchored.key() else {
                return Err(RejectError::new(
                    RejectKind::NonScalarKey,
                    "a mapping key aliases a collection, so it has no member name",
                )
                .at(self.location()));
            };
            self.begin_key(name, "", ScalarStyle::Plain, None);
        } else {
            self.place_anchored(anchored);
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

    /// Remember a collection an anchor names.
    fn remember(&mut self, anchor: Option<String>, value: &Value) {
        if let Some(anchor) = anchor {
            self.anchors.insert(
                anchor,
                Anchored::Node {
                    value: value.clone(),
                    key: None,
                },
            );
        }
    }

    /// Finish the innermost collection.
    fn close_frame(&mut self) -> (Option<String>, Value) {
        match self.frames.pop() {
            Some(Frame::Sequence { anchor, items, .. }) => (anchor, Value::Array(items)),
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

    /// Drop a value the model cannot hold, and say so.
    ///
    /// `why` finishes the sentence about `literal`, so `.inf` and a number beyond an `f64` say
    /// what each is.
    fn drop_unrepresentable(&mut self, literal: &str, why: &str) {
        let keyword = self.enclosing_key().map(ToOwned::to_owned);
        let validation = keyword
            .as_deref()
            .is_some_and(|key| VALIDATION_KEYWORDS.contains(&key));
        let location = self.location();
        let (action, detail) = if validation {
            (
                Action::Degrade,
                format!(
                    "`{literal}` {why}; ignored the constraint, so the generated type accepts \
                     values the document excludes"
                ),
            )
        } else {
            (
                Action::Repair,
                format!("`{literal}` {why}; dropped the value"),
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
            Some(Frame::Sequence { dropped, .. }) => *dropped += 1,
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
                Frame::Sequence { items, dropped, .. } => {
                    pointer.push((items.len() + dropped).to_string());
                }
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
///
/// A number is named by its canonical literal rather than by the value it was read as, so `1.50`
/// stays `1.50` whether or not the model holds the literal exactly, spelled the way arbitrary
/// precision renders it.
fn key_name(value: &Value, raw: &str) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => canonical_json_number(raw).map_or_else(
            || number.to_string(),
            |canonical| number::key_spelling(&canonical),
        ),
        // Collections never reach here; callers reject them first.
        Value::Array(_) | Value::Object(_) => raw.to_owned(),
    }
}

/// What an anchor names, so an alias of it is the same thing again, reported the same way.
#[derive(Clone)]
enum Anchored {
    /// A collection, or a scalar held as written, with the member name a scalar has as a key.
    Node { value: Value, key: Option<String> },
    /// A number the model holds only as this nearest value.
    Rounded {
        value: Value,
        literal: String,
        key: String,
    },
    /// A number the model cannot hold at all, for the reason `why` gives.
    Dropped {
        key: String,
        literal: String,
        why: &'static str,
    },
}

impl Anchored {
    /// The member name the node has as a key, or `None` for a collection, which has none.
    fn key(&self) -> Option<String> {
        match self {
            Self::Node { key, .. } => key.clone(),
            Self::Rounded { key, .. } | Self::Dropped { key, .. } => Some(key.clone()),
        }
    }
}

/// Why `.inf` and `.nan` are dropped.
const NOT_JSON: &str = "is not representable in JSON";

/// Why a number beyond an `f64` is dropped.
const BEYOND_RANGE: &str =
    "is beyond the numbers progeny holds without the `exact-numbers` feature";

enum Resolved {
    Value(Value),
    /// A number JSON cannot hold: `.inf`, `-.inf`, `.nan`.
    NonFinite,
    /// A number beyond the range the model holds without arbitrary precision.
    BeyondRange,
    /// A number the model holds only as this nearest value.
    Rounded(Number),
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
            "int" | "float" => number_or_text(text),
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
    number_or_text(text)
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

/// A YAML core-schema number, keeping its literal form wherever the model can, or the text when
/// it is not a number at all.
fn number_or_text(text: &str) -> Resolved {
    let Some(canonical) = canonical_json_number(text) else {
        return Resolved::Value(Value::String(text.to_owned()));
    };
    match number::hold(&canonical) {
        Held::Exact(number) => Resolved::Value(Value::Number(number)),
        Held::Rounded(number) => Resolved::Rounded(number),
        Held::Unrepresentable => Resolved::BeyondRange,
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
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{canonical_json_number, load};
    use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, RejectKind};

    fn yaml(text: &str) -> eyre::Result<Value> {
        let mut ctx = Ctx::new();
        Ok(load(text, &mut ctx)?)
    }

    fn yaml_with_diagnostics(text: &str) -> eyre::Result<(Value, Vec<Diagnostic>)> {
        let mut ctx = Ctx::new();
        let value = load(text, &mut ctx)?;
        Ok((value, ctx.into_diagnostics()))
    }

    #[test_util::test]
    fn integer_keys_become_their_canonical_string() {
        assert_eq!(
            yaml(indoc::indoc! {r#"
                responses:
                  200:
                    description: ok
                  "404":
                    description: no
            "#})?,
            json!({"responses": {"200": {"description": "ok"}, "404": {"description": "no"}}})
        );
    }

    #[test_util::test]
    fn every_scalar_kind_of_key_gets_a_canonical_name() {
        // `null` and `~` are the same key, so the later one wins, exactly as two identical keys
        // would. `0200` is the integer 200, and its canonical JSON rendering has no leading zero.
        assert_eq!(
            yaml(indoc::indoc! {"
                true: a
                null: b
                ~: c
                0200: d
                1.50: e
            "})?,
            json!({"true": "a", "null": "c", "200": "d", "1.50": "e"})
        );
    }

    #[test_util::test]
    fn core_schema_does_not_resolve_yaml_11_booleans() {
        // `figma` requires a property named `y`; `stytch` a JWK field named `n`. A YAML 1.1
        // loader turns these into booleans and corrupts the model.
        assert_eq!(
            yaml(indoc::indoc! {"
                required:
                  - x
                  - y
                  - n
                  - on
                  - off
                  - yes
                  - no
            "})?,
            json!({"required": ["x", "y", "n", "on", "off", "yes", "no"]})
        );
    }

    #[test_util::test]
    fn a_bare_equals_sign_is_a_string() {
        // `zendesk` has `change: =` inside an example. YAML 1.1 resolves `=` to the `!!value`
        // tag, which loaders without a constructor for it reject outright.
        assert_eq!(yaml("change: =\n")?, json!({"change": "="}));
    }

    /// Spelling survives only with arbitrary precision; without it the value does, which
    /// `numbers_the_model_rounds_are_reported` covers.
    #[test_util::test]
    fn number_literals_survive_exactly() {
        if !super::number::arbitrary_precision() {
            return Ok(());
        }
        // Every digit is kept: `1.0` does not collapse to `1`, `1.00` does not collapse to `1.0`,
        // and a float that only `f64` round-tripping would mangle keeps all seventeen digits. The
        // one thing that is normalized is the exponent's sign, which the value type writes
        // explicitly; it is the same number either way and it is deterministic.
        let value = yaml(indoc::indoc! {"
            a: 1
            b: 1.0
            c: 1.00
            d: 1e3
            e: 0.30000000000000004
        "})?;
        let text = serde_json::to_string(&value)?;
        assert_eq!(
            text,
            r#"{"a":1,"b":1.0,"c":1.00,"d":1e+3,"e":0.30000000000000004}"#
        );
    }

    /// Without arbitrary precision a number the model holds only roughly is reported where it
    /// sits, one too large to hold at all is dropped and reported, and a value it holds exactly,
    /// however spelled, is neither.
    /// An alias of either is reported again where it lands, even when the anchor is a key.
    #[test_util::test]
    fn numbers_the_model_rounds_are_reported() {
        if super::number::arbitrary_precision() {
            return Ok(());
        }
        let (value, diagnostics) = yaml_with_diagnostics(indoc::indoc! {"
            exact: 1.00
            spelled: 1e3
            long: 0.30000000000000000000001
            huge: 1e400
            keys:
              &big 18446744073709551617: anchored
              &far 1e400: anchored
            reused: *big
            again: *far
        "})?;
        assert_eq!(
            value,
            json!({
                "exact": 1.0,
                "spelled": 1000.0,
                "long": 0.3,
                "keys": {"18446744073709551617": "anchored", "1e+400": "anchored"},
                "reused": 18_446_744_073_709_551_617.0_f64,
            })
        );
        let classes: Vec<_> = diagnostics
            .iter()
            .map(|found| {
                let related: Vec<String> =
                    found.related().iter().map(ToString::to_string).collect();
                (found.class(), found.location().to_string(), related)
            })
            .collect();
        // Each finding aggregates, so an alias's occurrence is a related location of the first.
        assert_eq!(
            classes,
            [
                (
                    BreakageClass::InexactNumber,
                    "/long".to_owned(),
                    vec!["/reused".to_owned()]
                ),
                (
                    BreakageClass::NonFiniteNumber,
                    "/huge".to_owned(),
                    vec!["/again".to_owned()]
                ),
            ]
        );
    }

    /// A dropped element still takes its place in the document, so a later finding names the
    /// element where the document has it.
    #[test_util::test]
    fn a_dropped_element_keeps_later_locations() {
        let (value, diagnostics) = yaml_with_diagnostics("x: [.inf, 1.5, .nan]\n")?;
        assert_eq!(value, json!({"x": [1.5]}));
        let locations: Vec<String> = diagnostics
            .iter()
            .flat_map(|found| {
                std::iter::once(found.location().to_string())
                    .chain(found.related().iter().map(ToString::to_string))
            })
            .collect();
        // The `.nan` is the third element, though only one element survives before it.
        assert!(locations.contains(&"/x/2".to_owned()), "{locations:?}");
    }

    /// A number used as a key is a member name, so it keeps its spelling, through an alias
    /// too, with or without arbitrary precision.
    #[test_util::test]
    fn a_number_key_keeps_its_spelling() {
        let value = yaml(indoc::indoc! {"
            1.50: plain
            first: &spelled 2.50
            *spelled : aliased
            1e2: exponent
        "})?;
        assert_eq!(
            value
                .as_object()
                .map(|members| members.keys().cloned().collect::<Vec<_>>()),
            Some(vec![
                "1.50".to_owned(),
                "1e+2".to_owned(),
                "2.50".to_owned(),
                "first".to_owned()
            ])
        );
    }

    #[test_util::test]
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

    #[test_util::test]
    fn scalars_that_only_look_numeric_stay_strings() {
        for text in [
            ".", "-", "12:30", "1_000", "0x", "0o8", "1e", "1.2.3", "abc",
        ] {
            assert_eq!(canonical_json_number(text), None, "{text}");
        }
        assert_eq!(yaml("at: 12:30\n")?, json!({"at": "12:30"}));
    }

    #[test_util::test]
    fn quoted_scalars_are_never_resolved() {
        assert_eq!(
            yaml(indoc::indoc! {r#"
                a: "1"
                b: 'true'
                c: "null"
            "#})?,
            json!({"a": "1", "b": "true", "c": "null"})
        );
    }

    #[test_util::test]
    fn block_scalars_keep_their_text() {
        assert_eq!(
            yaml(indoc::indoc! {"
                a: |
                  one
                  two
                b: >-
                  folded
                  text
            "})?,
            json!({
                "a": indoc::indoc! {"
                    one
                    two
                "},
                "b": "folded text",
            })
        );
    }

    #[test_util::test]
    fn aliases_are_resolved_by_copying() {
        // `workos` and `openai` both use anchors, including on collections.
        let value = yaml(indoc::indoc! {"
            shared: &group
              - a
              - b
            first: *group
            second: *group
        "})?;
        assert_eq!(
            value,
            json!({"shared": ["a", "b"], "first": ["a", "b"], "second": ["a", "b"]})
        );
    }

    #[test_util::test]
    fn an_anchor_on_a_scalar_is_usable_as_a_key_and_a_value() {
        assert_eq!(
            yaml(indoc::indoc! {"
                name: &n title
                alias: *n
            "})?,
            json!({"name": "title", "alias": "title"})
        );
    }

    #[test_util::test]
    fn merge_keys_are_resolved_with_explicit_members_winning() {
        let value = yaml(indoc::indoc! {"
            base: &base
              type: string
              description: from base
            derived:
              <<: *base
              description: mine
        "})?;
        assert_eq!(
            value["derived"],
            json!({"type": "string", "description": "mine"})
        );
    }

    #[test_util::test]
    fn a_sequence_of_merge_sources_prefers_the_earlier_one() {
        let value = yaml(indoc::indoc! {"
            a: &a
              k: from a
            b: &b
              k: from b
              extra: yes
            merged:
              <<: [*a, *b]
        "})?;
        assert_eq!(value["merged"], json!({"k": "from a", "extra": "yes"}));
    }

    #[test_util::test]
    fn a_quoted_merge_key_is_an_ordinary_member() {
        assert_eq!(
            yaml(indoc::indoc! {r#"
                a:
                  "<<": x
            "#})?,
            json!({"a": {"<<": "x"}})
        );
    }

    #[test_util::test]
    fn a_non_scalar_key_is_rejected() {
        let mut ctx = Ctx::new();
        let error = load(
            indoc::indoc! {"
                ? [a, b]
                : value
            "},
            &mut ctx,
        )
        .err()
        .ok_or_eyre("the test expects this operation to fail")?;
        assert_eq!(error.kind(), RejectKind::NonScalarKey);
    }

    #[test_util::test]
    fn a_non_finite_annotation_drops_the_value() {
        let (value, diagnostics) = yaml_with_diagnostics(indoc::indoc! {"
            schema:
              default: .inf
              type: number
        "})?;
        assert_eq!(value, json!({"schema": {"type": "number"}}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::NonFiniteNumber);
        assert_eq!(diagnostics[0].action(), Action::Repair);
        assert_eq!(diagnostics[0].location().to_string(), "/schema/default");
    }

    #[test_util::test]
    fn a_non_finite_constraint_degrades_the_type() {
        let (value, diagnostics) = yaml_with_diagnostics(indoc::indoc! {"
            maximum: -.inf
            minimum: 0
        "})?;
        assert_eq!(value, json!({"minimum": 0}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].action(), Action::Degrade);
        assert_eq!(diagnostics[0].location().to_string(), "/maximum");
    }

    #[test_util::test]
    fn a_non_finite_sequence_element_is_dropped_with_its_position() {
        let (value, diagnostics) = yaml_with_diagnostics(indoc::indoc! {"
            enum:
              - 1
              - .nan
              - 3
        "})?;
        assert_eq!(value, json!({"enum": [1, 3]}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location().to_string(), "/enum/1");
    }

    #[test_util::test]
    fn duplicate_keys_keep_the_last_value() {
        assert_eq!(
            yaml(indoc::indoc! {"
                a: 1
                a: 2
            "})?,
            json!({"a": 2})
        );
    }

    #[test_util::test]
    fn an_empty_document_loads_as_null() {
        assert_eq!(yaml("")?, Value::Null);
        assert_eq!(yaml("# just a comment\n")?, Value::Null);
    }

    #[test_util::test]
    fn only_the_first_document_of_a_stream_is_loaded() {
        assert_eq!(
            yaml(indoc::indoc! {"
                a: 1
                ---
                b: 2
            "})?,
            json!({"a": 1})
        );
    }

    #[test_util::test]
    fn a_dangling_alias_is_a_rejection_rather_than_a_panic() {
        let mut ctx = Ctx::new();
        assert!(load("a: *nope\n", &mut ctx).is_err());
    }

    #[test_util::test]
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

        let error = outcome
            .err()
            .ok_or_eyre("the test expects this operation to fail")?;
        assert_eq!(error.kind(), RejectKind::Unparsable);
    }

    #[test_util::test]
    fn flow_style_is_the_same_document_as_block_style() {
        assert_eq!(
            yaml("{a: [1, 2], b: {c: d}}")?,
            yaml(indoc::indoc! {"
                a:
                  - 1
                  - 2
                b:
                  c: d
            "})?
        );
    }

    #[test_util::test]
    fn explicit_core_schema_tags_are_honoured() {
        assert_eq!(
            yaml(indoc::indoc! {r#"
                a: !!str 1
                b: !!int "2"
                c: !!bool "true"
                d: !!null ""
            "#})?,
            json!({"a": "1", "b": 2, "c": true, "d": null})
        );
    }
}
