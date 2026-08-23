//! Dialect normalization: OpenAPI 3.0 documents rewritten into 3.1 form, before parsing.
//!
//! 3.0's schema dialect is a modified draft-05 subset and 3.1's is JSON Schema 2020-12. The two
//! converge on **one lowering** by rewriting 3.0 into 3.1 form here, as a pure `Value → Value`
//! function: independently testable, and structurally incapable of the lossy mid-pipeline
//! conversion that a second parser would invite.
//!
//! The walk is **positional**: it descends through the places a document keeps schemas rather
//! than rewriting any object that happens to contain a `nullable` member. That distinction is
//! load-bearing. A pattern-matching rewrite would corrupt example payloads and vendor extensions
//! that contain such keys innocently — `nullable`, `example` and `format` are all ordinary words
//! that appear inside `example:` blocks all over the corpus.
//!
//! Two of the rewrites are **not** dialect conversions and run whatever version the document
//! declares, because the form they repair is invalid in every version: the draft-04 tuple
//! `items: [A, B]` (`webflow` writes 651 of them in a document declaring 3.1) and the boolean
//! `exclusiveMinimum` (`openai` writes 7). Version-gating a repair for a form that is never valid
//! would help nobody. Those two are diagnosed where they are defects; the dialect rows are silent,
//! because for a 3.0 document they are the documented normalization rather than a finding.

use serde_json::{Map, Value};

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer, RejectError, RejectKind};

/// A document in 3.1 form.
///
/// Constructible only by [`normalize`], which is how "3.0 never reaches the parser" is a
/// property of the type system rather than a convention: the parse entry point takes this, and
/// the only way to obtain one is to have gone through version detection and rewriting.
#[derive(Debug)]
pub(crate) struct Normalized {
    value: Value,
    /// Whether the document declared 3.0, kept because one rewrite happens after this phase:
    /// a schema at a position nothing parsed is normalized when a `$ref` proves it is one.
    dialect_30: bool,
    #[cfg(any(feature = "harness", test))]
    version: Version,
}

impl Normalized {
    #[cfg(feature = "harness")]
    pub(crate) fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }

    pub(crate) fn dialect_30(&self) -> bool {
        self.dialect_30
    }

    #[cfg(any(feature = "harness", test))]
    pub(crate) fn version(&self) -> &Version {
        &self.version
    }
}

/// The `openapi` version a document declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    /// Exactly what the document wrote, kept because it is part of the document.
    pub(crate) text: String,
}

/// Detect the dialect and rewrite it into 3.1 form.
pub(crate) fn normalize(value: Value, ctx: &mut Ctx) -> Result<Normalized, RejectError> {
    let Value::Object(mut root) = value else {
        return Err(RejectError::new(
            RejectKind::NotAnObject,
            "an OpenAPI document is a JSON object; this document's root is not one",
        ));
    };

    let version = version(&root)?;
    if version.major != 3 {
        return Err(RejectError::new(
            RejectKind::UnsupportedVersion,
            format!(
                "`openapi: {}` declares major version {}; progeny implements OpenAPI 3",
                version.text, version.major
            ),
        )
        .at(JsonPointer::root().child("openapi")));
    }
    if !root.contains_key("paths") && !root.contains_key("webhooks") {
        return Err(RejectError::new(
            RejectKind::NoOperations,
            "the document has neither `paths` nor `webhooks`, so it describes no operations",
        ));
    }

    // A minor version from the future is read as 3.1 rather than rejected: 3.1 is the dialect the
    // parser implements, the model is a superset that holds members it does not interpret, and a
    // document is more useful read imperfectly than not at all.
    if version.minor > 1 {
        ctx.report(Diagnostic::new(
            BreakageClass::UnsupportedDialect,
            Action::Warn,
            JsonPointer::root().child("openapi"),
            format!(
                "`openapi: {}` is newer than 3.1; read it as 3.1, so members this version adds \
                 are preserved but not interpreted",
                version.text
            ),
        ));
    }

    if let Some(Value::String(dialect)) = root.get("jsonSchemaDialect")
        && !is_known_dialect(dialect)
    {
        report_dialect(ctx, JsonPointer::root().child("jsonSchemaDialect"), dialect);
    }

    let dialect_30 = version.minor == 0;
    Walk {
        ctx,
        at: JsonPointer::root(),
        dialect_30,
    }
    .document(&mut root);

    Ok(Normalized {
        value: Value::Object(root),
        dialect_30,
        #[cfg(any(feature = "harness", test))]
        version,
    })
}

/// Rewrite one schema that the positional walk did not reach.
///
/// The walk descends through the positions a document keeps schemas, and deliberately refuses to
/// guess at anything else — rewriting an object merely because it holds a `nullable` member is how
/// example payloads get corrupted. A `$ref` addressing a position outside those is the evidence
/// that turns the guess into a fact: something reads that node as a schema, so it is one, and
/// [`crate::resolve`] normalizes it here before parsing it.
pub(crate) fn schema_at(value: &mut Value, dialect_30: bool, at: JsonPointer, ctx: &mut Ctx) {
    Walk {
        ctx,
        at,
        dialect_30,
    }
    .schema(value);
}

fn version(root: &Map<String, Value>) -> Result<Version, RejectError> {
    let Some(declared) = root.get("openapi") else {
        return Err(RejectError::new(
            RejectKind::MissingVersion,
            "the document has no `openapi` member, so its version is unknown",
        ));
    };
    // YAML resolves an unquoted `3.0` to a number, so the member is a number in real documents
    // as often as it deserves to be a string.
    let text = match declared {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => {
            return Err(RejectError::new(
                RejectKind::MissingVersion,
                format!("`openapi` should be a version string; it is {other}"),
            )
            .at(JsonPointer::root().child("openapi")));
        }
    };

    let mut components = text.trim().split('.');
    let major = components.next().and_then(|part| part.parse::<u32>().ok());
    let minor = components
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let Some(major) = major else {
        return Err(RejectError::new(
            RejectKind::MissingVersion,
            format!("`openapi: {text}` is not a version number"),
        )
        .at(JsonPointer::root().child("openapi")));
    };

    Ok(Version { major, minor, text })
}

/// The path-item members that hold operations, as the wire spells them.
///
/// This module sits below `doc` and walks raw values, so it cannot ask the document model; the
/// mirror test beside `doc::serialize` pins this list to `PathItem::operations` instead, because
/// a method missing here is a method whose schemas silently skip every rewrite.
pub(crate) const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Every place a subschema can appear under exactly one key.
///
/// The three lists below are the model's applicators spelled as keywords, and the test beside
/// `schema::serialize`'s maximal fixture holds them to that — a keyword the model holds that
/// these lists miss is a position whose rewrites silently never run.
pub(crate) const SUBSCHEMA: [&str; 11] = [
    "additionalProperties",
    "contains",
    "contentSchema",
    "else",
    "if",
    "items",
    "not",
    "propertyNames",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
];

/// Every place an array of subschemas can appear.
pub(crate) const SUBSCHEMA_ARRAY: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// Every place a name-to-subschema map can appear. `definitions` is not an OpenAPI keyword, but
/// tools that lower JSON Schema into 3.0 emit it, and descending into it costs nothing.
pub(crate) const SUBSCHEMA_MAP: [&str; 5] = [
    "$defs",
    "definitions",
    "dependentSchemas",
    "patternProperties",
    "properties",
];

/// Which kind of node a `components` section holds.
///
/// A named section rather than a function pointer so that the walk's recursion stays visible in
/// the source: every arm below is a place a schema can hide.
#[derive(Clone, Copy)]
enum Section {
    Schemas,
    Responses,
    /// Parameters and headers are the same node bar the `name` and `in` members.
    Parameters,
    RequestBodies,
    Callbacks,
    PathItems,
}

impl Section {
    fn of(name: &str) -> Option<Self> {
        match name {
            "schemas" => Some(Self::Schemas),
            "responses" => Some(Self::Responses),
            "parameters" | "headers" => Some(Self::Parameters),
            "requestBodies" => Some(Self::RequestBodies),
            "callbacks" => Some(Self::Callbacks),
            "pathItems" => Some(Self::PathItems),
            // `examples`, `links` and `securitySchemes` hold no schemas.
            _ => None,
        }
    }
}

/// The positional walk: where it is, and which rewrites apply.
struct Walk<'a> {
    ctx: &'a mut Ctx,
    at: JsonPointer,
    /// Whether the document declares 3.0, in which case the dialect rows apply and their
    /// rewriting is the documented normalization rather than a finding worth reporting.
    dialect_30: bool,
}

impl Walk<'_> {
    /// Descend into `token`, restoring the location afterwards.
    fn scoped(&mut self, token: &str, visit: impl FnOnce(&mut Self)) {
        self.at.push(token);
        visit(self);
        self.at.pop();
    }

    fn report(&mut self, class: BreakageClass, at: JsonPointer, detail: &str) {
        self.ctx
            .report(Diagnostic::new(class, Action::Repair, at, detail));
    }

    fn document(&mut self, root: &mut Map<String, Value>) {
        for section in ["paths", "webhooks"] {
            if let Some(Value::Object(items)) = root.get_mut(section) {
                self.scoped(section, |walk| {
                    for (key, item) in items.iter_mut() {
                        if !is_extension(key) {
                            walk.scoped(key, |walk| walk.path_item(item));
                        }
                    }
                });
            }
        }
        if let Some(Value::Object(components)) = root.get_mut("components") {
            self.scoped("components", |walk| {
                for (key, entries) in components.iter_mut() {
                    let Value::Object(entries) = entries else {
                        continue;
                    };
                    let Some(section) = Section::of(key) else {
                        continue;
                    };
                    walk.scoped(key, |walk| {
                        for (name, entry) in entries.iter_mut() {
                            walk.scoped(name, |walk| walk.section(section, entry));
                        }
                    });
                }
            });
        }
    }

    fn section(&mut self, section: Section, value: &mut Value) {
        match section {
            Section::Schemas => self.schema(value),
            Section::Responses => self.response(value),
            Section::Parameters => self.parameter(value),
            Section::RequestBodies => self.request_body(value),
            Section::Callbacks => self.callback(value),
            Section::PathItems => self.path_item(value),
        }
    }

    fn path_item(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        self.parameter_array(map);
        for method in METHODS {
            if let Some(entry) = map.get_mut(method) {
                self.scoped(method, |walk| walk.operation(entry));
            }
        }
    }

    fn operation(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        self.parameter_array(map);
        if let Some(entry) = map.get_mut("requestBody") {
            self.scoped("requestBody", |walk| walk.request_body(entry));
        }
        for (key, section) in [
            ("responses", Section::Responses),
            ("callbacks", Section::Callbacks),
        ] {
            if let Some(Value::Object(entries)) = map.get_mut(key) {
                self.scoped(key, |walk| {
                    for (name, entry) in entries.iter_mut() {
                        if !is_extension(name) {
                            walk.scoped(name, |walk| walk.section(section, entry));
                        }
                    }
                });
            }
        }
    }

    fn parameter_array(&mut self, map: &mut Map<String, Value>) {
        if let Some(Value::Array(parameters)) = map.get_mut("parameters") {
            self.scoped("parameters", |walk| {
                for (index, entry) in parameters.iter_mut().enumerate() {
                    walk.scoped(&index.to_string(), |walk| walk.parameter(entry));
                }
            });
        }
    }

    fn callback(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        for (key, entry) in map.iter_mut() {
            if !is_extension(key) {
                self.scoped(key, |walk| walk.path_item(entry));
            }
        }
    }

    fn parameter(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        if let Some(entry) = map.get_mut("schema") {
            self.scoped("schema", |walk| walk.schema(entry));
        }
        self.content(map);
    }

    fn request_body(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        self.content(map);
    }

    fn response(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        self.header_map(map);
        self.content(map);
    }

    fn header_map(&mut self, map: &mut Map<String, Value>) {
        if let Some(Value::Object(headers)) = map.get_mut("headers") {
            self.scoped("headers", |walk| {
                for (name, entry) in headers.iter_mut() {
                    walk.scoped(name, |walk| walk.parameter(entry));
                }
            });
        }
    }

    fn content(&mut self, map: &mut Map<String, Value>) {
        let Some(Value::Object(media_types)) = map.get_mut("content") else {
            return;
        };
        self.scoped("content", |walk| {
            for (name, entry) in media_types.iter_mut() {
                walk.scoped(name, |walk| walk.media_type(entry));
            }
        });
    }

    fn media_type(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        if let Some(entry) = map.get_mut("schema") {
            self.scoped("schema", |walk| walk.schema(entry));
        }
        if let Some(Value::Object(encodings)) = map.get_mut("encoding") {
            self.scoped("encoding", |walk| {
                for (name, entry) in encodings.iter_mut() {
                    let Value::Object(encoding) = entry else {
                        continue;
                    };
                    walk.scoped(name, |walk| walk.header_map(encoding));
                }
            });
        }
    }

    /// Rewrite one schema and descend into its subschemas.
    ///
    /// Boolean schemas and anything that is not an object have nothing to rewrite.
    fn schema(&mut self, value: &mut Value) {
        let Value::Object(map) = value else {
            return;
        };
        // A schema may declare its own dialect (`$schema`); that member is read and reported by
        // the schema parser, which is the layer that stores it. Reporting it here too once made
        // one member into two records.
        if self.dialect_30 {
            // Before `nullable` runs on the branches themselves and takes the evidence away.
            self.repair_null_branches(map);
            nullable(map);
            exclusive_bounds(map);
            example(map);
            string_format(map);
        } else {
            self.repair_exclusive_bounds(map);
            self.repair_string_format(map);
        }
        self.repair_tuple_items(map);
        self.repair_required_flags(map);

        for key in SUBSCHEMA {
            if let Some(entry) = map.get_mut(key) {
                self.scoped(key, |walk| walk.schema(entry));
            }
        }
        for key in SUBSCHEMA_ARRAY {
            if let Some(Value::Array(entries)) = map.get_mut(key) {
                self.scoped(key, |walk| {
                    for (index, entry) in entries.iter_mut().enumerate() {
                        walk.scoped(&index.to_string(), |walk| walk.schema(entry));
                    }
                });
            }
        }
        for key in SUBSCHEMA_MAP {
            if let Some(Value::Object(entries)) = map.get_mut(key) {
                self.scoped(key, |walk| {
                    for (name, entry) in entries.iter_mut() {
                        walk.scoped(name, |walk| walk.schema(entry));
                    }
                });
            }
        }
    }

    /// A 3.0 union branch whose only content is `nullable: true`.
    ///
    /// 3.0 has no `"null"` type, so this is the only spelling available for the null arm of a
    /// union, and it is the one tools generating 3.0 emit. Read literally it is something else
    /// entirely: `nullable` modifies "the defined schema", a branch that declares no `type` defines
    /// nothing, and a branch constraining nothing accepts every payload its siblings do — which
    /// costs the whole union its type. `qdrant` writes exactly this 277 times in one response.
    ///
    /// Repair rather than degrade, and the distinction is the point: the literal reading makes the
    /// union meaningless, so the evident intent is not a guess between two readings but the only
    /// one under which the document says anything at all.
    fn repair_null_branches(&mut self, map: &mut Map<String, Value>) {
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(branches)) = map.get_mut(key) else {
                continue;
            };
            for (index, branch) in branches.iter_mut().enumerate() {
                let Value::Object(branch) = branch else {
                    continue;
                };
                if !is_bare_nullable(branch) {
                    continue;
                }
                branch.remove("nullable");
                branch.insert("type".to_owned(), Value::String("null".to_owned()));
                let location = self.at.child(key).child(index.to_string());
                self.report(
                    BreakageClass::NullableUnionBranch,
                    location,
                    "this branch of the union says only `nullable: true`, which is 3.0's one \
                     spelling for the null arm; read it as `type: null`, because the literal \
                     reading is a branch that constrains nothing and would cost the union its type",
                );
            }
        }
    }

    /// The draft-04 tuple form, in a document of any version.
    ///
    /// `items: [A, B]` constrains an array positionally; 2020-12 spells that `prefixItems`, and
    /// reads an array in `items` as an error. This is the corpus's single largest source of
    /// findings — 652 occurrences, all in documents declaring 3.1 — and it is the row that takes
    /// tuples from a rarity worth degrading to real support worth having.
    fn repair_tuple_items(&mut self, map: &mut Map<String, Value>) {
        if !matches!(map.get("items"), Some(Value::Array(_))) {
            return;
        }
        // A document writing both spellings is saying two different things; rewriting would
        // discard one of them, so leave it for the parser to preserve verbatim and diagnose.
        if map.contains_key("prefixItems") {
            return;
        }
        let location = self.at.child("items");
        if let Some(tuple) = map.remove("items") {
            map.insert("prefixItems".to_owned(), tuple);
        }
        // draft-04's `additionalItems` constrains the elements past the tuple, which is exactly
        // what 2020-12's `items` means once `prefixItems` is present.
        if let Some(rest) = map.remove("additionalItems") {
            map.insert("items".to_owned(), rest);
        }
        self.report(
            BreakageClass::LegacyTupleItems,
            location,
            "`items` holds an array of schemas, which is the draft-04 tuple form; rewrote it as \
             `prefixItems` (and any `additionalItems` as `items`), so the element types are kept",
        );
    }

    /// The draft-03 per-property `required` flag, in a document of any version.
    ///
    /// draft-03 marked a property required on the property itself; every draft since, and both
    /// OpenAPI dialects, say it with an array on the parent. Read literally the flag is a member no
    /// keyword claims, so the requirement is simply dropped and every member of the type becomes
    /// optional — a document saying less than it meant, with nothing to show for it. `fincrm`
    /// writes 76 of them in a document declaring 3.0.
    ///
    /// Repair rather than degrade: the intent of `required: true` on a property has one reading,
    /// and it is the reading the array on the parent spells.
    fn repair_required_flags(&mut self, map: &mut Map<String, Value>) {
        let mut flagged = Vec::new();
        let mut required = Vec::new();
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for (name, property) in properties.iter_mut() {
                let Value::Object(property) = property else {
                    continue;
                };
                if !matches!(property.get("required"), Some(Value::Bool(_))) {
                    continue;
                }
                flagged.push(name.clone());
                if property.remove("required") == Some(Value::Bool(true)) {
                    required.push(Value::String(name.clone()));
                }
            }
        }
        if flagged.is_empty() {
            return;
        }
        // Only when the parent has an array there, or has nothing there at all. A parent already
        // writing something else in `required` is saying two contradictory things, and merging
        // into one of them would discard the other; the parser preserves it verbatim and
        // diagnoses it.
        match map.get_mut("required") {
            Some(Value::Array(existing)) => {
                for name in required {
                    if !existing.contains(&name) {
                        existing.push(name);
                    }
                }
            }
            None => {
                if !required.is_empty() {
                    map.insert("required".to_owned(), Value::Array(required));
                }
            }
            Some(_) => {}
        }
        for name in flagged {
            // The detail carries no values: two occurrences of one finding have to render
            // identically or they cannot be aggregated into one record.
            self.report(
                BreakageClass::LegacyRequiredFlag,
                self.at.child("properties").child(name).child("required"),
                "`required` is a boolean, which is draft-03's way of marking the property itself; \
                 every dialect since says it with an array on the parent, so it was moved there",
            );
        }
    }

    /// The 3.0 boolean bound flag, in a document that does not declare 3.0.
    ///
    /// Identical rewriting to [`exclusive_bounds`], but reported: in a 3.0 document this is the
    /// documented normalization, and anywhere else it is a defect that would otherwise leave the
    /// bound uninterpretable.
    fn repair_exclusive_bounds(&mut self, map: &mut Map<String, Value>) {
        for (flag, bound) in exclusive_bounds(map) {
            // The detail carries no values: two occurrences of one finding have to render
            // identically or they cannot be aggregated into one record.
            self.report(
                BreakageClass::LegacyExclusiveBound,
                self.at.child(flag),
                &format!(
                    "`{flag}` is a boolean, which is the 3.0 form modifying a sibling `{bound}`; \
                     2020-12 has no boolean there, so it was rewritten as the numeric bound"
                ),
            );
        }
    }

    /// `format: byte` or `format: binary` inside a document declaring 3.1.
    ///
    /// 2020-12 removed both — the facts moved to `contentEncoding` and `contentMediaType` — so a
    /// 3.1 document writing them is writing the 3.0 spelling, and reading it as one loses nothing.
    /// The same argument as the boolean `exclusiveMinimum` repair beside it, and the corpus says
    /// the same thing twice: **110 occurrences across 15 documents that declare 3.1**, `telnyx`,
    /// `openai` and `langsmith` at the head of them.
    ///
    /// It matters most for a multipart body, where `format: binary` is the only thing marking
    /// *which property* is a file — a fact the media-type key cannot carry. Left unread, those
    /// members become text parts, which is a request built wrong rather than a type named oddly.
    fn repair_string_format(&mut self, map: &mut Map<String, Value>) {
        let Some(already) = string_format(map) else {
            return;
        };
        // The detail carries no values: two occurrences of one finding have to render identically
        // or they cannot be aggregated into one record.
        self.report(
            BreakageClass::LegacyStringFormat,
            self.at.child("format"),
            if already {
                "`format` is the 3.0 spelling of a fact 2020-12 keeps elsewhere, and the 2020-12 \
                 member is already present; the `format` was dropped"
            } else {
                "`format` is the 3.0 spelling of a fact 2020-12 moved to another member; 2020-12 \
                 defines no such format, so it was rewritten as that member"
            },
        );
    }
}

/// Whether a declared dialect is one progeny reads as itself.
///
/// 2020-12 and OpenAPI 3.1's own base dialect, with or without the empty fragment both are written
/// with. Everything else is read *as* 2020-12 anyway — the model holds keywords it does not
/// interpret — so the finding is a warning about a possible disagreement rather than a refusal.
///
/// The one list for both positions a dialect can be declared in: the document's
/// `jsonSchemaDialect`, checked here, and a schema's own `$schema`, checked by the schema parser.
/// Two copies of this list once drifted into two differently worded reports of one member.
pub(crate) fn is_known_dialect(declared: &str) -> bool {
    const KNOWN: [&str; 2] = [
        "https://json-schema.org/draft/2020-12/schema",
        "https://spec.openapis.org/oas/3.1/dialect/base",
    ];
    KNOWN.contains(&declared.trim_end_matches('#'))
}

fn report_dialect(ctx: &mut Ctx, at: JsonPointer, declared: &str) {
    ctx.report(Diagnostic::new(
        BreakageClass::UnsupportedDialect,
        Action::Warn,
        at,
        format!(
            "`{declared}` is not a dialect progeny implements; the schema is read as 2020-12, so \
             any keyword the two dialects disagree about is held but not interpreted"
        ),
    ));
}

/// Whether a schema declares itself nullable and says nothing else about the value.
///
/// The test is an allowlist of the members that annotate rather than constrain, so a keyword
/// progeny has not considered means "this branch says something", and the repair does not fire.
/// Getting that direction wrong would rewrite a branch that had real content into `type: null`.
fn is_bare_nullable(map: &Map<String, Value>) -> bool {
    const ANNOTATIONS: [&str; 10] = [
        "default",
        "deprecated",
        "description",
        "example",
        "externalDocs",
        "readOnly",
        "title",
        "writeOnly",
        "xml",
        "nullable",
    ];
    if map.get("nullable") != Some(&Value::Bool(true)) {
        return false;
    }
    map.keys()
        .all(|key| ANNOTATIONS.contains(&key.as_str()) || key.starts_with("x-"))
}

/// `nullable: true` becomes `"null"` in the type — and in the enum.
///
/// Adding `"null"` to `type` alone would be a silent narrowing wherever an enum is also present:
/// in 3.1 an instance must satisfy both, so `type: [string, "null"]` with `enum: [a, b]` rejects
/// the very `null` the document was asking to allow.
fn nullable(map: &mut Map<String, Value>) {
    match map.get("nullable") {
        Some(Value::Bool(true)) => {}
        // `nullable: false` is 3.1's default and says nothing there.
        Some(Value::Bool(false)) => {
            map.remove("nullable");
            return;
        }
        // Absent, or a value that is not a boolean: leave it for the parser to preserve and
        // diagnose rather than guessing what it meant.
        _ => return,
    }
    map.remove("nullable");

    match map.get_mut("type") {
        Some(Value::String(name)) => {
            let name = std::mem::take(name);
            map.insert(
                "type".to_owned(),
                Value::Array(vec![Value::String(name), Value::String("null".to_owned())]),
            );
        }
        Some(Value::Array(names)) if !names.iter().any(|name| name == "null") => {
            names.push(Value::String("null".to_owned()));
        }
        Some(_) => {}
        // With no `type` of its own, a bare annotation schema narrows nothing and there is
        // nothing to widen — but a composition sibling does narrow. `{"nullable": true,
        // "allOf": [{"$ref": X}]}` is precisely the spelling 3.0 generators emit for a
        // nullable reference (a 3.0 `$ref` cannot carry siblings, so tools wrap it), and
        // dropping the flag there silently narrowed away the `null` the document allows on
        // a member the vendor may document `null` as meaningful for. The composition moves
        // into one branch of an `anyOf` beside an explicit `{"type": "null"}` branch — the
        // 3.1 nullable-emulation form classification already reads as an `Option`. The
        // annotations and any non-composition constraints stay where they were: `null`
        // satisfies a string- or object-scoped constraint vacuously, so leaving them
        // conjunct does not narrow the null branch away.
        None => {
            let mut composition = Map::new();
            for keyword in ["$ref", "allOf", "oneOf", "anyOf"] {
                if let Some(member) = map.remove(keyword) {
                    composition.insert(keyword.to_owned(), member);
                }
            }
            if !composition.is_empty() {
                let mut null_branch = Map::new();
                null_branch.insert("type".to_owned(), Value::String("null".to_owned()));
                map.insert(
                    "anyOf".to_owned(),
                    Value::Array(vec![Value::Object(composition), Value::Object(null_branch)]),
                );
            }
        }
    }

    if let Some(Value::Array(values)) = map.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
}

/// 3.0's boolean `exclusiveMinimum`/`exclusiveMaximum` modify a sibling bound; 3.1's are the
/// bound.
/// The one boolean-bound rewrite, returning the flags it rewrote.
///
/// Shared by the 3.0 path, where it is the documented normalization and silent, and by the
/// wrong-dialect repair, whose caller reports each returned flag. One table, one transformation:
/// these were two copies once, and a third spelling added to one of them would have left 3.0
/// documents — the ones the rewrite exists for — silently unrepaired.
fn exclusive_bounds(map: &mut Map<String, Value>) -> Vec<(&'static str, &'static str)> {
    let mut rewritten = Vec::new();
    for (flag, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        let Some(Value::Bool(exclusive)) = map.get(flag) else {
            continue;
        };
        // `exclusive*: false` means the sibling bound is inclusive, which is what a bare bound
        // already means, so the flag is simply dropped.
        let exclusive = *exclusive;
        map.remove(flag);
        if exclusive && let Some(value) = map.remove(bound) {
            map.insert(flag.to_owned(), value);
        }
        rewritten.push((flag, bound));
    }
    rewritten
}

/// 3.0's schema-level `example` is 3.1's `examples`, which is an array.
fn example(map: &mut Map<String, Value>) {
    if map.contains_key("examples") {
        return;
    }
    if let Some(value) = map.remove("example") {
        map.insert("examples".to_owned(), Value::Array(vec![value]));
    }
}

/// 3.0 says "this string is base64" and "this string is bytes" with `format`; 3.1 says the first
/// with `contentEncoding` and the second at the media-type level, which is where 3.0 and 3.1
/// genuinely differ about *where* the fact lives.
///
/// The one table and the one rewrite, shared by the silent 3.0 path and the reported
/// wrong-dialect repair; `Some(already)` says a rewrite happened and whether the 2020-12 member
/// was already present (the `format` is then dropped rather than moved), which is exactly the
/// split the repair's wording needs.
fn string_format(map: &mut Map<String, Value>) -> Option<bool> {
    if !type_includes(map, "string") {
        return None;
    }
    let (key, value) = match map.get("format").and_then(Value::as_str)? {
        "byte" => ("contentEncoding", "base64"),
        // 3.1's replacement for `format: binary`. Not simply dropped: a multipart body marks
        // *which property* is a file this way, and the media-type key cannot carry a
        // per-property fact.
        "binary" => ("contentMediaType", "application/octet-stream"),
        _ => return None,
    };
    map.remove("format");
    let already = map.contains_key(key);
    map.entry(key.to_owned())
        .or_insert_with(|| Value::String(value.to_owned()));
    Some(already)
}

fn type_includes(map: &Map<String, Value>, name: &str) -> bool {
    match map.get("type") {
        Some(Value::String(declared)) => declared == name,
        Some(Value::Array(declared)) => declared.iter().any(|entry| entry == name),
        _ => false,
    }
}

fn is_extension(key: &str) -> bool {
    key.starts_with("x-")
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::normalize;
    use crate::diag::{Action, BreakageClass, Ctx, RejectKind};

    fn normalized(value: Value) -> eyre::Result<Value> {
        let mut ctx = Ctx::new();
        Ok(normalize(value, &mut ctx)?.into_value())
    }

    /// A 3.0 document with one schema at `components.schemas.S`.
    fn with_schema(schema: &Value) -> Value {
        json!({"openapi": "3.0.3", "paths": {}, "components": {"schemas": {"S": schema}}})
    }

    /// The same, declaring 3.1, where the dialect rows do not apply.
    fn with_schema_31(schema: &Value) -> Value {
        json!({"openapi": "3.1.0", "paths": {}, "components": {"schemas": {"S": schema}}})
    }

    fn normalized_schema(schema: &Value) -> eyre::Result<Value> {
        Ok(normalized(with_schema(schema))?["components"]["schemas"]["S"].clone())
    }

    /// Normalize a 3.1 document with one schema, keeping the diagnostics.
    fn normalized_schema_31(schema: &Value) -> eyre::Result<(Value, Vec<crate::Diagnostic>)> {
        let mut ctx = Ctx::new();
        let out = normalize(with_schema_31(schema), &mut ctx)?.into_value();
        Ok((
            out["components"]["schemas"]["S"].clone(),
            ctx.into_diagnostics(),
        ))
    }

    #[test_util::test]
    fn a_31_document_is_left_alone() {
        let original = json!({
            "openapi": "3.1.0",
            "paths": {},
            "components": {"schemas": {"S": {"type": ["string", "null"], "example": 1}}},
        });
        assert_eq!(normalized(original.clone())?, original);
    }

    #[test_util::test]
    fn nullable_becomes_a_null_type_in_array_form() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": true}))?,
            json!({"type": ["string", "null"]})
        );
        assert_eq!(
            normalized_schema(&json!({"type": ["string"], "nullable": true}))?,
            json!({"type": ["string", "null"]})
        );
        assert_eq!(
            normalized_schema(&json!({"type": ["string", "null"], "nullable": true}))?,
            json!({"type": ["string", "null"]})
        );
    }

    #[test_util::test]
    fn nullable_also_widens_an_enum() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "enum": ["a", "b"], "nullable": true}))?,
            json!({"type": ["string", "null"], "enum": ["a", "b", null]})
        );
    }

    #[test_util::test]
    fn nullable_without_a_type_narrows_nothing() {
        assert_eq!(
            normalized_schema(&json!({"nullable": true, "description": "d"}))?,
            json!({"description": "d"})
        );
    }

    #[test_util::test]
    fn nullable_beside_a_composition_becomes_a_null_branch() {
        // `allOf` — the spelling 3.0 generators emit for a nullable reference, since a 3.0
        // `$ref` cannot carry siblings.
        assert_eq!(
            normalized_schema(
                &json!({"nullable": true, "allOf": [{"$ref": "#/components/schemas/T"}]})
            )?,
            json!({"anyOf": [
                {"allOf": [{"$ref": "#/components/schemas/T"}]},
                {"type": "null"},
            ]})
        );
        // A bare `$ref` sibling: ignored by 3.0's letter, but the flag is unambiguous intent.
        assert_eq!(
            normalized_schema(&json!({"nullable": true, "$ref": "#/components/schemas/T"}))?,
            json!({"anyOf": [{"$ref": "#/components/schemas/T"}, {"type": "null"}]})
        );
        assert_eq!(
            normalized_schema(&json!({"nullable": true, "oneOf": [{"type": "string"}]}))?,
            json!({"anyOf": [{"oneOf": [{"type": "string"}]}, {"type": "null"}]})
        );
        assert_eq!(
            normalized_schema(&json!({"nullable": true, "anyOf": [{"type": "string"}]}))?,
            json!({"anyOf": [{"anyOf": [{"type": "string"}]}, {"type": "null"}]})
        );
    }

    #[test_util::test]
    fn annotations_stay_beside_the_null_branch_wrapper() {
        assert_eq!(
            normalized_schema(&json!({
                "nullable": true,
                "description": "d",
                "allOf": [{"$ref": "#/components/schemas/T"}],
            }))?,
            json!({
                "description": "d",
                "anyOf": [
                    {"allOf": [{"$ref": "#/components/schemas/T"}]},
                    {"type": "null"},
                ],
            })
        );
    }

    #[test_util::test]
    fn nullable_false_is_the_31_default() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": false}))?,
            json!({"type": "string"})
        );
    }

    #[test_util::test]
    fn a_non_boolean_nullable_is_left_for_the_parser_to_diagnose() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "nullable": "yes"}))?,
            json!({"type": "string", "nullable": "yes"})
        );
    }

    #[test_util::test]
    fn boolean_exclusive_bounds_become_numeric_ones() {
        assert_eq!(
            normalized_schema(&json!({"minimum": 0, "exclusiveMinimum": true}))?,
            json!({"exclusiveMinimum": 0})
        );
        assert_eq!(
            normalized_schema(&json!({"maximum": 10, "exclusiveMaximum": false}))?,
            json!({"maximum": 10})
        );
        // A flag with nothing to modify is just noise.
        assert_eq!(
            normalized_schema(&json!({"exclusiveMinimum": true}))?,
            json!({})
        );
    }

    #[test_util::test]
    fn schema_level_example_becomes_examples() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "example": "a"}))?,
            json!({"type": "string", "examples": ["a"]})
        );
        // An already-3.1 `examples` wins; the stray `example` is left for the parser to keep.
        assert_eq!(
            normalized_schema(&json!({"examples": ["a"], "example": "b"}))?,
            json!({"examples": ["a"], "example": "b"})
        );
    }

    #[test_util::test]
    fn string_formats_move_to_where_31_says_them() {
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "byte"}))?,
            json!({"type": "string", "contentEncoding": "base64"})
        );
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "binary"}))?,
            json!({"type": "string", "contentMediaType": "application/octet-stream"})
        );
        // An explicit `contentMediaType` wins; normalization never overwrites
        // what the document already said.
        assert_eq!(
            normalized_schema(
                &json!({"type": "string", "format": "binary", "contentMediaType": "image/png"})
            )?,
            json!({"type": "string", "contentMediaType": "image/png"})
        );
        // `format: binary` on something that is not a string is not the 3.0 idiom.
        assert_eq!(
            normalized_schema(&json!({"type": "object", "format": "binary"}))?,
            json!({"type": "object", "format": "binary"})
        );
        assert_eq!(
            normalized_schema(&json!({"type": "string", "format": "date-time"}))?,
            json!({"type": "string", "format": "date-time"})
        );
    }

    #[test_util::test]
    fn rewrites_reach_every_subschema_position() {
        let rewritten = normalized_schema(&json!({
            "allOf": [{"type": "string", "nullable": true}],
            "properties": {"a": {"type": "integer", "nullable": true}},
            "items": {"type": "boolean", "nullable": true},
            "additionalProperties": {"type": "number", "nullable": true},
            "$defs": {"D": {"type": "string", "nullable": true}},
        }))?;
        assert_eq!(rewritten["allOf"][0]["type"], json!(["string", "null"]));
        assert_eq!(
            rewritten["properties"]["a"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(rewritten["items"]["type"], json!(["boolean", "null"]));
        assert_eq!(
            rewritten["additionalProperties"]["type"],
            json!(["number", "null"])
        );
        assert_eq!(rewritten["$defs"]["D"]["type"], json!(["string", "null"]));
    }

    #[test_util::test]
    fn rewrites_reach_schemas_under_paths_and_responses() {
        let document = json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "parameters": [{"name": "a", "in": "query", "schema": {"type": "string", "nullable": true}}],
                    "post": {
                        "requestBody": {"content": {"application/json": {"schema": {"type": "string", "nullable": true}}}},
                        "responses": {
                            "200": {
                                "description": "ok",
                                "headers": {"X-A": {"schema": {"type": "string", "nullable": true}}},
                                "content": {"application/json": {"schema": {"type": "integer", "nullable": true}}},
                            },
                        },
                    },
                },
            },
        });
        let out = normalized(document)?;
        let path = &out["paths"]["/pets"];
        assert_eq!(
            path["parameters"][0]["schema"]["type"],
            json!(["string", "null"])
        );
        let post = &path["post"];
        assert_eq!(
            post["requestBody"]["content"]["application/json"]["schema"]["type"],
            json!(["string", "null"])
        );
        let response = &post["responses"]["200"];
        assert_eq!(
            response["headers"]["X-A"]["schema"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            response["content"]["application/json"]["schema"]["type"],
            json!(["integer", "null"])
        );
    }

    #[test_util::test]
    fn decoys_outside_schema_positions_are_untouched() {
        // Every one of these is an ordinary word inside a payload, not a keyword. A rewrite
        // that pattern-matched on member names would corrupt all of them.
        let document = json!({
            "openapi": "3.0.1",
            "x-defaults": {"nullable": true, "type": "string", "example": "keep me"},
            "paths": {
                "/a": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "d",
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object"},
                                        "example": {"nullable": true, "type": "string", "format": "byte"},
                                        "examples": {
                                            "one": {"value": {"nullable": true, "minimum": 1, "exclusiveMinimum": true}},
                                        },
                                    },
                                },
                            },
                        },
                    },
                },
            },
            "components": {
                "schemas": {
                    "S": {
                        "type": "object",
                        "properties": {
                            "nullable": {"type": "boolean"},
                            "example": {"type": "string"},
                        },
                        "default": {"nullable": true, "example": 1},
                        "examples": [{"nullable": true, "format": "binary", "type": "string"}],
                    },
                },
            },
        });
        assert_eq!(normalized(document.clone())?, document);
    }

    #[test_util::test]
    fn the_draft_04_tuple_form_becomes_prefix_items_whatever_the_version_says() {
        let (rewritten, diagnostics) = normalized_schema_31(&json!({
            "type": "array",
            "items": [{"type": "string"}, {"type": "integer"}],
        }))?;
        assert_eq!(
            rewritten,
            json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}]})
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::LegacyTupleItems);
        assert_eq!(diagnostics[0].action(), Action::Repair);
        assert_eq!(
            diagnostics[0].location().to_string(),
            "/components/schemas/S/items"
        );

        // A 3.0 document writes the same defect, and gets the same repair.
        assert_eq!(
            normalized_schema(&json!({"items": [{"type": "string"}]}))?,
            json!({"prefixItems": [{"type": "string"}]})
        );
    }

    #[test_util::test]
    fn a_draft_03_required_flag_moves_onto_the_parent() {
        let (rewritten, diagnostics) = normalized_schema_31(&json!({
            "type": "object",
            "required": ["already"],
            "properties": {
                "already": {"type": "string"},
                "name": {"type": "string", "required": true},
                "nickname": {"type": "string", "required": false},
            },
        }))?;
        assert_eq!(
            rewritten,
            json!({
                "type": "object",
                "required": ["already", "name"],
                "properties": {
                    "already": {"type": "string"},
                    "name": {"type": "string"},
                    "nickname": {"type": "string"},
                },
            })
        );
        // Both flags are findings — `required: false` says nothing the name's absence from the
        // array does not, but it is still a member of a dialect this document does not speak —
        // and the class aggregates, so the two occurrences are one record.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::LegacyRequiredFlag);
        assert_eq!(diagnostics[0].action(), Action::Repair);
        assert_eq!(
            diagnostics[0].location().to_string(),
            "/components/schemas/S/properties/name/required"
        );

        // A 3.0 document writes the same defect, and gets the same repair.
        assert_eq!(
            normalized_schema(&json!({"properties": {"a": {"required": true}}}))?,
            json!({"required": ["a"], "properties": {"a": {}}})
        );
    }

    #[test_util::test]
    fn a_parent_whose_required_is_neither_an_array_nor_absent_keeps_what_it_wrote() {
        let (rewritten, diagnostics) = normalized_schema_31(&json!({
            "type": "object",
            "required": "name",
            "properties": {"name": {"type": "string", "required": true}},
        }))?;
        assert_eq!(
            rewritten,
            json!({
                "type": "object",
                "required": "name",
                "properties": {"name": {"type": "string"}},
            })
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::LegacyRequiredFlag);
    }

    #[test_util::test]
    fn additional_items_becomes_the_constraint_on_the_rest() {
        let (rewritten, _) = normalized_schema_31(&json!({
            "items": [{"type": "string"}],
            "additionalItems": false,
        }))?;
        assert_eq!(
            rewritten,
            json!({"prefixItems": [{"type": "string"}], "items": false})
        );
    }

    #[test_util::test]
    fn a_document_writing_both_tuple_spellings_is_left_for_the_parser() {
        let both = json!({"items": [{"type": "string"}], "prefixItems": [{"type": "integer"}]});
        let (rewritten, diagnostics) = normalized_schema_31(&both)?;
        assert_eq!(rewritten, both);
        assert!(diagnostics.is_empty());
    }

    #[test_util::test]
    fn the_tuple_repair_reaches_nested_positions_and_aggregates() {
        let (rewritten, diagnostics) = normalized_schema_31(&json!({
            "type": "object",
            "properties": {
                "a": {"items": [{"type": "string"}]},
                "b": {"items": [{"type": "integer"}]},
            },
        }))?;
        assert!(rewritten["properties"]["a"]["prefixItems"].is_array());
        assert!(rewritten["properties"]["b"]["prefixItems"].is_array());
        // One record for the document, not one per site: this class fires 651 times in `webflow`.
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].occurrences().get(), 2);
        assert_eq!(
            diagnostics[0].location().to_string(),
            "/components/schemas/S/properties/a/items"
        );
        assert_eq!(
            diagnostics[0].related()[0].to_string(),
            "/components/schemas/S/properties/b/items"
        );
    }

    #[test_util::test]
    fn a_boolean_bound_flag_outside_30_is_repaired_and_reported() {
        let (rewritten, diagnostics) = normalized_schema_31(&json!({
            "type": "integer",
            "minimum": 0,
            "exclusiveMinimum": true,
        }))?;
        assert_eq!(rewritten, json!({"type": "integer", "exclusiveMinimum": 0}));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::LegacyExclusiveBound);
        assert_eq!(diagnostics[0].action(), Action::Repair);

        // The same rewriting in a 3.0 document is the documented normalization, so it says
        // nothing: a finding there would fire on every well-formed 3.0 document in the corpus.
        let mut ctx = Ctx::new();
        normalize(
            with_schema(&json!({"minimum": 0, "exclusiveMinimum": true})),
            &mut ctx,
        )?;
        assert!(ctx.into_diagnostics().is_empty());
    }

    #[test_util::test]
    fn a_31_document_with_nothing_to_repair_is_untouched() {
        // The walk now visits 3.1 documents too, so the decoys have to hold there as well.
        let document = json!({
            "openapi": "3.1.0",
            "x-defaults": {"items": [1, 2], "exclusiveMinimum": true},
            "paths": {
                "/a": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "d",
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object", "nullable": true},
                                        "example": {"items": [1, 2], "exclusiveMinimum": true},
                                    },
                                },
                            },
                        },
                    },
                },
            },
            "components": {
                "schemas": {
                    "S": {
                        "type": "object",
                        "properties": {"items": {"type": "array"}},
                        "examples": [{"items": [1], "exclusiveMinimum": true}],
                        "default": {"items": [1]},
                    },
                },
            },
        });
        let mut ctx = Ctx::new();
        let out = normalize(document.clone(), &mut ctx)?.into_value();
        assert_eq!(out, document);
        assert!(ctx.into_diagnostics().is_empty());
    }

    #[test_util::test]
    fn a_property_named_like_a_keyword_is_still_a_schema() {
        let rewritten = normalized_schema(&json!({
            "type": "object",
            "properties": {"nullable": {"type": "string", "nullable": true}},
        }))?;
        assert_eq!(
            rewritten["properties"]["nullable"]["type"],
            json!(["string", "null"])
        );
    }

    #[test_util::test]
    fn a_yaml_number_version_is_a_version() {
        let value: Value = serde_json::from_str(r#"{"openapi": 3.0, "paths": {}}"#)?;
        let mut ctx = Ctx::new();
        let out = normalize(value, &mut ctx)?;
        assert_eq!(out.version().major, 3);
        assert_eq!(out.version().minor, 0);
        assert_eq!(out.version().text, "3.0");
    }

    #[test_util::test]
    fn the_declared_version_string_is_not_rewritten() {
        let out = normalized(json!({"openapi": "3.0.2", "paths": {}}))?;
        assert_eq!(out["openapi"], json!("3.0.2"));
    }

    #[test_util::test]
    fn a_future_minor_version_is_read_as_31_with_a_warning() {
        let mut ctx = Ctx::new();
        normalize(json!({"openapi": "3.2.0", "paths": {}}), &mut ctx)?;
        let diagnostics = ctx.into_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].class(), BreakageClass::UnsupportedDialect);
        assert_eq!(diagnostics[0].action(), Action::Warn);
    }

    #[test_util::test]
    fn documents_that_cannot_be_used_are_rejected() {
        let mut ctx = Ctx::new();
        for (value, kind) in [
            (json!([]), RejectKind::NotAnObject),
            (json!({"paths": {}}), RejectKind::MissingVersion),
            (
                json!({"openapi": "x", "paths": {}}),
                RejectKind::MissingVersion,
            ),
            (
                json!({"openapi": "2.0", "paths": {}}),
                RejectKind::UnsupportedVersion,
            ),
            (
                json!({"openapi": "4.0.0", "paths": {}}),
                RejectKind::UnsupportedVersion,
            ),
            (json!({"openapi": "3.1.0"}), RejectKind::NoOperations),
        ] {
            assert_eq!(
                normalize(value.clone(), &mut ctx)
                    .err()
                    .ok_or_eyre("the test expects this operation to fail")?
                    .kind(),
                kind,
                "{value}"
            );
        }
    }

    #[test_util::test]
    fn webhooks_alone_describe_operations() {
        let mut ctx = Ctx::new();
        assert!(normalize(json!({"openapi": "3.1.0", "webhooks": {}}), &mut ctx).is_ok());
    }

    #[test_util::test]
    fn normalization_is_idempotent() {
        let once = normalized(with_schema(&json!({
            "type": "string",
            "nullable": true,
            "format": "byte",
            "example": "a",
            "minimum": 1,
            "exclusiveMinimum": true,
        })))?;
        let mut ctx = Ctx::new();
        let twice = normalize(once.clone(), &mut ctx)?.into_value();
        assert_eq!(once, twice);
    }
}
