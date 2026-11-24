//! The one body an operation sends: which media type, and what contract it becomes.

use std::collections::{BTreeMap, BTreeSet};

use super::Build;
use super::param::param_shape;
use crate::api::style::{self, ParamShape};
use crate::api::{BodyContract, FormSpec, PartKind, PartSpec, Style, reading};
use crate::contract::{
    ContractKind, Contracts, FieldContract, Format, SkipRule, TypeContract, TypeIndex, TypeRef,
};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MediaType, Operation};
use crate::schema::Schema;

/// A request body position, lowered once per declared media type.
///
/// The primary is the media type the fixed preference picks — or the one
/// [`Config::media_types`](crate::config::Config::media_types) names for this position — and it
/// keeps the operation's own name. Every other declared type becomes a client method of its own,
/// because which body to *send* is the caller's decision and a position that declares two ways to
/// upload has declared two operations in every sense a caller cares about.
pub(super) struct BodyVariants {
    pub(super) primary: BodyContract,
    /// The other declared media types, in preference order: each is rendered as a suffixed
    /// sibling method.
    pub(super) variants: Vec<(String, BodyContract)>,
}

impl Build<'_> {
    /// The bodies the operation can send: one per declared media type.
    pub(super) fn bodies(
        &mut self,
        operation: &Operation,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<BodyVariants> {
        let node = operation.request_body.as_ref()?;
        let at = at.child("requestBody");
        let Some(body) = self.resolved.request_body(node) else {
            // A request body reference that resolves to nothing. The operation keeps its body and
            // sends arbitrary JSON, which is the same rule a dangling response gets and for the
            // same reason: the position degrades, the operation stays.
            ctx.report(Diagnostic::new(
                BreakageClass::DanglingRef,
                Action::Degrade,
                at,
                "the request body references a component the document does not declare; the body \
                 is typed as arbitrary JSON",
            ));
            return Some(BodyVariants {
                primary: BodyContract::Json {
                    ty: TypeRef::Value,
                    required: false,
                    content_type: "application/json".to_owned(),
                    accepts: None,
                },
                variants: Vec::new(),
            });
        };
        let required = body.required.unwrap_or(false);
        let content = body.content.as_ref()?;
        let position = at.child("content").to_string();
        if !self.config.media_types.is_empty() {
            self.content_positions
                .insert(position.clone(), content.keys().cloned().collect());
        }
        let configured = self
            .config
            .media_types
            .get(&position)
            .and_then(|requested| content.keys().find(|declared| *declared == requested));
        let primary_key = match configured {
            Some(configured) => configured,
            None => content
                .keys()
                .min_by_key(|media_type| (preference(media_type), media_type.as_str()))?,
        };

        // Alternates, minus what cannot or need not become a method: a wildcard is not a content
        // type a client can send, and a second spelling of a base already generated — a `charset`
        // parameter, usually — would be the same method twice.
        let mut bases = vec![base_of(primary_key)];
        let mut variant_keys: Vec<&String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut ordered: Vec<&String> = content.keys().filter(|key| *key != primary_key).collect();
        ordered.sort_by_key(|media_type| (preference(media_type), media_type.as_str()));
        for key in ordered {
            let base = base_of(key);
            if base.contains('*') {
                skipped.push(format!("`{key}` is a wildcard"));
            } else if bases.contains(&base) {
                skipped.push(format!("`{key}` repeats a media type already generated"));
            } else {
                bases.push(base);
                variant_keys.push(key);
            }
        }
        if !variant_keys.is_empty() || !skipped.is_empty() {
            let skipped = if skipped.is_empty() {
                String::new()
            } else {
                format!("; {} and is not generated", skipped.join(", and "))
            };
            ctx.report(Diagnostic::new(
                BreakageClass::MultiMediaType,
                Action::Degrade,
                at.child("content"),
                format!(
                    "the position declares {} media types; each is generated as its own client \
                     method, and the generated server accepts only `{primary_key}`{skipped}",
                    content.len(),
                ),
            ));
        }

        let primary = self.body_of(primary_key, &content[primary_key], required, &at, ctx);
        let variants = variant_keys
            .into_iter()
            .map(|key| {
                (
                    key.clone(),
                    self.body_of(key, &content[key], required, &at, ctx),
                )
            })
            .collect();
        Some(BodyVariants { primary, variants })
    }

    fn body_of(
        &mut self,
        media_type: &str,
        entry: &MediaType,
        required: bool,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> BodyContract {
        let ty = entry.schema.map_or(TypeRef::Value, |id| self.type_at(id));
        let base = media_type
            .split(';')
            .next()
            .unwrap_or(media_type)
            .trim()
            .to_ascii_lowercase();
        if is_json(&base) {
            return BodyContract::Json {
                ty,
                required,
                content_type: media_type.trim().to_owned(),
                accepts: None,
            };
        }
        match base.as_str() {
            "application/x-www-form-urlencoded" => BodyContract::Form {
                specs: self.form_specs(&ty, entry, at, ctx),
                reading: reading::form(&ty, self.contracts),
                ty,
                required,
            },
            "multipart/form-data" => {
                // The parts and the server's reading come from the type the body sends, so the
                // form that holds the uploads has to be chosen before either is derived.
                let ty = self.contracts.multipart_form(&ty);
                let parts = self.part_specs(&ty, entry, ctx);
                BodyContract::Multipart {
                    reading: reading::multipart(&ty, &parts, self.contracts),
                    parts,
                    ty,
                    required,
                }
            }
            // A wildcard is not a content type, and a body with nothing but a wildcard has no
            // other to fall back to — `preference` only sorts it last. Sending `*/*` as a header
            // would be sending something no server can act on.
            //
            // Which one to send instead is decided by the schema, because a wildcard permits every
            // content type and only one of them matches what the document typed. `telnyx` writes
            // `*/*` over a `$ref` to a real object, and sending that as bytes would throw away a
            // type the document gave; `jellyfin` writes `image/*` over a binary string, where
            // bytes is exactly right.
            wildcard if wildcard.contains('*') => {
                let structured = !matches!(
                    ty,
                    TypeRef::Format(Format::Binary | Format::Base64) | TypeRef::String
                ) && entry.schema.is_some();
                let chosen = if structured {
                    "application/json"
                } else {
                    "application/octet-stream"
                };
                ctx.report(Diagnostic::new(
                    BreakageClass::MultiMediaType,
                    Action::Degrade,
                    at.child("content"),
                    format!(
                        "`{media_type}` is the only media type the body declares, and a wildcard \
                         is not a content type to send; the body is sent as `{chosen}`, which is \
                         the one the declared schema fits"
                    ),
                ));
                if structured {
                    BodyContract::Json {
                        ty,
                        required,
                        content_type: "application/json".to_owned(),
                        accepts: Some(wildcard.to_owned()),
                    }
                } else {
                    BodyContract::Bytes {
                        content_type: chosen.to_owned(),
                        required,
                        accepts: Some(wildcard.to_owned()),
                    }
                }
            }
            // Text is a `String` rather than bytes, because that is what a caller has: making them
            // spell `.into_bytes()` buys nothing and loses the fact that the body is text.
            text if text.starts_with("text/") => BodyContract::Text {
                content_type: media_type.to_owned(),
                required,
            },
            _ => BodyContract::Bytes {
                content_type: media_type.to_owned(),
                required,
                accepts: None,
            },
        }
    }

    /// One spec per declared member of a form body: the row from `encoding` where the document
    /// wrote one, and the member's shape from the contract always.
    ///
    /// The first version carried only the `encoding`-named members — one body in the whole corpus —
    /// which left the reader shape-blind for everything else, and the wire probe caught the cost on
    /// `posthog`: a one-element array member arrives as one occurrence, byte-identical to a scalar,
    /// and was handed to serde as its element. The shape lives in the contract either way; now the
    /// reader gets it for every member, the same way a query parameter does.
    fn form_specs(
        &self,
        ty: &TypeRef,
        entry: &MediaType,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Vec<FormSpec> {
        let mut declared: BTreeMap<String, (Style, bool)> = BTreeMap::new();
        for (name, encoding) in entry.encoding.iter().flatten() {
            if encoding.style.is_none() && encoding.explode.is_none() {
                continue;
            }
            let at = at.child("content").child("encoding").child(name);
            // The same classifier a query parameter goes through, because a form body is a query
            // string in the body position and an undefined combination is undefined in both.
            if let Some((style, explode)) = style::form_member(encoding, &at, ctx) {
                declared.insert(name.clone(), (style, explode));
            }
        }

        let mut specs = Vec::new();
        if let TypeRef::Named(index) = ty
            && let Some(contract) = self.contracts.get(*index)
            && let ContractKind::Struct { fields } = contract.kind()
        {
            // A captured `additionalProperties` field is not a member, as in `part_specs`: its
            // entries arrive under their own names and fall back to the repetition heuristic.
            for field in fields.iter().filter(|field| field.capture.is_none()) {
                let (style, explode) = declared
                    .remove(field.wire_name.as_str())
                    .unwrap_or((Style::Form, true));
                specs.push(FormSpec {
                    wire_name: field.wire_name.clone(),
                    style,
                    explode,
                    array: Some(matches!(
                        param_shape(&field.ty, self.contracts),
                        ParamShape::Array
                    )),
                    object: Some(matches!(
                        param_shape(&field.ty, self.contracts),
                        ParamShape::Object
                    )),
                });
            }
        }
        // `encoding` entries that name no declared member — a captured `additionalProperties`
        // member can still carry a row — keep the reach the encoding-only version had. Nothing
        // here knows the member's shape, and the row says so rather than guessing: a guessed
        // `false` once made the reader drop every repeated occurrence after the first, which is a
        // regression bought by *adding* an `encoding` entry to a document.
        for (name, (style, explode)) in declared {
            specs.push(FormSpec {
                wire_name: name,
                style,
                explode,
                array: None,
                object: None,
            });
        }
        specs
    }

    /// What the document was specific about, member by member, for a multipart body.
    ///
    /// Read from the *contract* rather than from the schema: the contract is the frozen record of
    /// what the type actually carries, and a part list derived from anything else could describe a
    /// member the emitted type does not have.
    ///
    /// A member whose explicit `null` the writer sends as absent is reported here, beside the part
    /// it becomes, as [`BreakageClass::MultipartNull`].
    fn part_specs(&mut self, ty: &TypeRef, entry: &MediaType, ctx: &mut Ctx) -> Vec<PartSpec> {
        let declared: BTreeMap<&str, &str> = entry
            .encoding
            .iter()
            .flatten()
            .filter_map(|(name, encoding)| Some((name.as_str(), encoding.content_type.as_deref()?)))
            .collect();
        // A nullable body is an `Option` of the struct and a cycle-broken one a `Box` of it;
        // the parts belong to the struct either way.
        let mut ty = ty;
        while let TypeRef::Option(inner) | TypeRef::Boxed(inner) = ty {
            ty = inner;
        }
        let TypeRef::Named(index) = ty else {
            return Vec::new();
        };
        let Some(contract) = self.contracts.get(*index) else {
            return Vec::new();
        };
        let ContractKind::Struct { fields } = contract.kind() else {
            return Vec::new();
        };
        // A captured `additionalProperties` field is not a member: its entries arrive under their
        // own names, which no row can describe, and its empty wire name would claim a part
        // literally named `""`.
        fields
            .iter()
            .filter(|field| field.capture.is_none())
            .map(|field| {
                // A `contentType` naming several types — `cloudflare` writes ten of them separated
                // by commas — is a list of what the server accepts, not an instruction about what
                // to send. Only a single type is a decision.
                let content_type = declared
                    .get(field.wire_name.as_str())
                    .filter(|declared| !declared.contains(','))
                    .map(|declared| (*declared).to_owned());
                let json_declared = content_type.as_deref().is_some_and(is_json);
                let part = part_of(&field.ty, json_declared, self.contracts);
                let lost = lost_nulls(field, part, self.contracts);
                if let Some(body) = entry.schema
                    && !lost.is_empty()
                {
                    // Where the member is declared, so the records of one document fold by what is
                    // lost. Without a declaration among this body's own schemas, the body itself,
                    // naming the member: never another operation's schema.
                    let (location, detail) = match self.member(entry, &field.wire_name) {
                        Some(member) => (member, "a multipart member".to_owned()),
                        // Where the body's schema was written, which a referenced request body
                        // has too.
                        None => (
                            self.resolved.schemas().address(body).clone(),
                            format!("the multipart member `{}`", field.wire_name),
                        ),
                    };
                    for lost in lost {
                        if self.null_members.insert((
                            location.clone(),
                            field.wire_name.clone(),
                            lost,
                        )) {
                            ctx.report(Diagnostic::new(
                                BreakageClass::MultipartNull,
                                Action::Degrade,
                                location.clone(),
                                format!("{detail} can hold an explicit `null`, and {lost}"),
                            ));
                        }
                    }
                }
                PartSpec {
                    kind: part.kind,
                    repeated: part.repeated,
                    keeps_text: part.keeps_text,
                    content_type,
                    wire_name: field.wire_name.clone(),
                }
            })
            .collect()
    }

    /// Where the body this operation sends declares the member `name`, among the schemas its own
    /// type is built from.
    ///
    /// Equal schemas share one type, whose origin can be another operation's schema, so the type's
    /// origin is never used.
    fn member(&self, entry: &MediaType, name: &str) -> Option<JsonPointer> {
        let declaring = entry.schema.and_then(|body| {
            crate::shape::key_of(self.resolved, body)
                .parts()
                .iter()
                .map(|part| part.id())
                .find(|&id| {
                    matches!(self.resolved.schema(id), Schema::Object(object)
                        if object.properties.as_ref().is_some_and(|properties| properties.contains_key(name)))
                })
        });
        declaring.map(|id| {
            self.resolved
                .schemas()
                .address(id)
                .child("properties")
                .child(name)
        })
    }

    /// Which media type a response position decodes: the configured pick, or the fixed
    /// preference.
    ///
    /// One, not one per declared type, which is the opposite of the rule a request body gets —
    /// deliberately. Which body to *send* is the caller's decision, so a request position's
    /// media types each become a method; which body *arrives* is the server's, so a response
    /// position gets a single decode and [`Config::media_types`](crate::config::Config::media_types)
    /// is how a caller states what it will `Accept`. The alternates are reported rather than
    /// silently dropped, and the report names the pointer a configuration entry would use to
    /// choose differently. A configured pick that names a media type this position does not
    /// declare falls through to the preference; generation still stops, because the walk records
    /// every position it visits and an unhonoured entry is rejected once the walk is done.
    pub(super) fn select<'a>(
        &mut self,
        content: &'a BTreeMap<String, MediaType>,
        at: &JsonPointer,
        accept: &mut Vec<String>,
        ctx: &mut Ctx,
    ) -> Option<(&'a str, &'a MediaType)> {
        let position = at.child("content").to_string();
        if !self.config.media_types.is_empty() {
            self.content_positions
                .insert(position.clone(), content.keys().cloned().collect());
        }
        let configured = self
            .config
            .media_types
            .get(&position)
            .and_then(|requested| content.keys().find(|declared| *declared == requested));
        let chosen = match configured {
            Some(configured) => configured,
            None => content
                .keys()
                .min_by_key(|media_type| (preference(media_type), media_type.as_str()))?,
        };
        // A configured pick is only an argument until the request carries it: the response media
        // type is the server's choice, and `Accept` is how the client's side of it is said.
        if configured.is_some() && !accept.contains(chosen) {
            accept.push(chosen.clone());
        }
        if content.len() > 1 {
            let others: Vec<&str> = content
                .keys()
                .filter(|other| *other != chosen)
                .map(String::as_str)
                .collect();
            ctx.report(Diagnostic::new(
                BreakageClass::MultiMediaType,
                Action::Degrade,
                at.child("content"),
                format!(
                    "the position declares {} media types; `{chosen}` is generated and {} {} not",
                    content.len(),
                    others
                        .iter()
                        .map(|other| format!("`{other}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if others.len() == 1 { "is" } else { "are" }
                ),
            ));
        }
        content
            .get_key_value(chosen)
            .map(|(name, entry)| (name.as_str(), entry))
    }
}

/// The media type without its parameters, lowercased: what decides whether two spellings are one
/// type.
fn base_of(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase()
}

/// The method-name suffix a body variant carries: `upload_document` sends the primary, and
/// `upload_document_multipart` is the same operation sending `multipart/form-data`.
///
/// The common types get the word a reader would use for them; anything else falls back to the
/// subtype — its `+suffix` when it has one, which is what `application/vnd.api+json` is *for* —
/// sanitized into an identifier. Collisions with a name the document declares are the
/// [`Namer`](crate::contract::Namer)'s to resolve, the same as everywhere else.
pub(super) fn method_suffix(media_type: &str) -> String {
    let base = base_of(media_type);
    match base.as_str() {
        "multipart/form-data" => return "multipart".to_owned(),
        "application/x-www-form-urlencoded" => return "form".to_owned(),
        "application/octet-stream" => return "bytes".to_owned(),
        "text/plain" => return "text".to_owned(),
        _ => {}
    }
    let subtype = base.split('/').next_back().unwrap_or(&base);
    let subtype = subtype.rsplit('+').next().unwrap_or(subtype);
    let sanitized: String = subtype
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        return format!("m{sanitized}");
    }
    sanitized
}

/// Where a media type sits in the preference order. Lower wins.
///
/// A **wildcard is always last**, whatever it wildcards over. `jellyfin` declares
/// `application/*+json` beside `application/json` on 75 request bodies, and a client that picked
/// the wildcard would have to send `*` as a content type — which is not a content type. It is a
/// perfectly good thing for a document to *say* about a response and never a thing to send.
fn preference(media_type: &str) -> u8 {
    let base = media_type.split(';').next().unwrap_or(media_type).trim();
    if base.contains('*') {
        return 9;
    }
    if base == "application/json" {
        return 0;
    }
    if is_json(base) {
        return 1;
    }
    match base {
        "application/x-www-form-urlencoded" => 2,
        "multipart/form-data" => 3,
        "application/octet-stream" => 4,
        "text/plain" => 6,
        other if other.starts_with("image/") || other.starts_with("audio/") => 5,
        _ => 7,
    }
}

/// What one member of a multipart body becomes, and whether it becomes several.
///
/// The kind is the *item's* kind, which is 3.1's rule: "an array — the default is defined based on
/// the type of the item". 3.0 said `application/json` for any array, and following the newer rule
/// for both dialects is deliberate — sending a repeated member as repeated parts is what every
/// multipart parser expects, and 3.0's reading would put a JSON array where a server looks for
/// several fields.
///
/// `PartKind::File` mirrors the type exactly: a member is a file part when and only when the
/// type layer gave it `support::Upload`, because the server-side parser re-wraps file parts into
/// the upload object form and only that type's `Deserialize` reads it — a `File` spec over a
/// `String`-typed member would make the generated server reject its own client. A binary member
/// the type layer left as `String`, such as a base64 (`format: byte`) member, is a text part
/// whose bytes are the string's, with the stated consequence that such a part cannot carry
/// invalid UTF-8.
///
/// A recursive member, such as `Tree(Vec<Tree>)`, is a JSON part: its structure has no faithful
/// text form.
fn part_of(ty: &TypeRef, json_declared: bool, contracts: &Contracts) -> Part {
    part_within(
        ty,
        Walk {
            json_declared,
            contracts,
        },
        &mut BTreeSet::new(),
    )
}

/// What every step of [`part_within`] reads.
#[derive(Clone, Copy)]
struct Walk<'a> {
    /// Whether the member's encoding declares a JSON `contentType`.
    json_declared: bool,
    contracts: &'a Contracts,
}

impl Walk<'_> {
    /// The part a string on the wire is written as: its text, or its JSON where the encoding
    /// declares JSON, so that a part whose header says JSON holds a JSON value.
    fn string(self) -> Part {
        if self.json_declared {
            Part::JSON
        } else {
            Part::text(true)
        }
    }
}

/// What one member of a multipart body becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Part {
    kind: PartKind,
    /// Whether the member is a sequence, written as one part per element.
    repeated: bool,
    /// Whether each part's text is the value itself, because what one part holds is a string on
    /// the wire: [`PartSpec::keeps_text`].
    keeps_text: bool,
}

impl Part {
    /// One part of JSON.
    const JSON: Self = Self {
        kind: PartKind::Json,
        repeated: false,
        keeps_text: false,
    };

    /// One part written as its text, which is the value itself when `string`.
    fn text(string: bool) -> Self {
        Self {
            kind: PartKind::Text,
            repeated: false,
            keeps_text: string,
        }
    }
}

/// [`part_of`], knowing the named types the walk has already looked into.
///
/// Each step follows exactly one wrapped type, so the set is also the walk's path, and a type met
/// again is one that holds itself.
/// A union's branches are walked apart, each with its own copy of the path, because two branches
/// naming the same type are not a cycle.
fn part_within(ty: &TypeRef, walk: Walk<'_>, followed: &mut BTreeSet<TypeIndex>) -> Part {
    match ty {
        TypeRef::Upload => Part {
            kind: PartKind::File,
            repeated: false,
            keeps_text: false,
        },
        // An option or a presence says nothing about the shape on the wire; a box says nothing at
        // all.
        TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Boxed(inner) => {
            part_within(inner, walk, followed)
        }
        // The one wrapper that does say something: each element is its own part.
        // An element that is itself a sequence is written as its JSON text, which is no string.
        TypeRef::Vec(inner) | TypeRef::Array(inner, _) => {
            let element = part_within(inner, walk, followed);
            Part {
                kind: element.kind,
                repeated: true,
                keeps_text: element.keeps_text && !element.repeated,
            }
        }
        // A number or a boolean is written as its text, which is also its JSON, so the encoding
        // declaring JSON changes no byte of a value.
        // It makes the part JSON all the same, because only a JSON part carries an explicit `null`.
        TypeRef::Bool | TypeRef::Integer(_) | TypeRef::F64 if walk.json_declared => Part::JSON,
        // Raw binary is no string: a text part cannot carry bytes that are not UTF-8.
        TypeRef::Bool | TypeRef::Integer(_) | TypeRef::F64 | TypeRef::Format(Format::Binary) => {
            Part::text(false)
        }
        // Base64 text is the transfer encoding of the content the declared type describes, so
        // quoting it would corrupt that content rather than make it JSON.
        TypeRef::Format(Format::Base64) => Part::text(true),
        TypeRef::String | TypeRef::Format(_) => walk.string(),
        TypeRef::Named(index) if !followed.insert(*index) => Part::JSON,
        // A string enum is a string on the wire, an alias is what it names, and a newtype is the
        // one value it wraps.
        TypeRef::Named(index) => match walk.contracts.get(*index).map(TypeContract::kind) {
            Some(ContractKind::StringEnum { .. }) => walk.string(),
            Some(ContractKind::Alias { target }) => part_within(target, walk, followed),
            Some(ContractKind::Newtype { inner }) => {
                let wrapped = part_within(inner, walk, followed);
                match wrapped.kind {
                    PartKind::Text if !wrapped.repeated => wrapped,
                    PartKind::Text | PartKind::File | PartKind::Json => Part::JSON,
                }
            }
            // A union of strings is a string too, whichever branch holds it, so its branches are
            // asked what they are on the wire rather than how the encoding writes them.
            // A union that may hold a number or a boolean beside a string is JSON: as text, `5`
            // the number and `"5"` the string would be the same part.
            Some(ContractKind::Enum { variants, .. }) => {
                let bare = Walk {
                    json_declared: false,
                    ..walk
                };
                let strings = variants.iter().all(|variant| {
                    part_within(&variant.ty, bare, &mut followed.clone()) == Part::text(true)
                });
                if strings { walk.string() } else { Part::JSON }
            }
            Some(
                ContractKind::Struct { .. }
                | ContractKind::TaggedEnum { .. }
                | ContractKind::CarriedTagEnum { .. }
                | ContractKind::KeyedEnum { .. }
                | ContractKind::Tuple { .. },
            )
            | None => Part::JSON,
        },
        // A map, a tuple or a degraded `Value` is structured, and structured members have no
        // faithful text form.
        TypeRef::Unit | TypeRef::Value | TypeRef::Map(_) | TypeRef::Tuple(_) => Part::JSON,
    }
}

/// Each way an explicit `null` the member can hold is lost on the way into its parts.
fn lost_nulls(field: &FieldContract, part: Part, contracts: &Contracts) -> Vec<&'static str> {
    let mut lost = Vec::new();
    if sends_null(field, contracts) {
        if part.repeated {
            lost.push("a `null` list has no spelling as repeated parts, so it is sent as absent");
        } else if part.kind != PartKind::Json {
            lost.push("a text or file part cannot carry a `null`, so it is sent as absent");
        }
    }
    if part.repeated
        && part.kind != PartKind::Json
        && element_of(&field.ty, contracts).is_some_and(|element| holds_null(element, contracts))
    {
        lost.push("a `null` element has no spelling as a text or file part, so it is left out");
    }
    lost
}

/// Whether the member's serialized form can write an explicit `null` for the member itself.
///
/// The skip rule decides: a member left out when `None` writes no `null` for that `None`, and a
/// presence-preserving one writes `null` for its null state.
fn sends_null(field: &FieldContract, contracts: &Contracts) -> bool {
    match (field.skip_serializing_if, &field.ty) {
        (SkipRule::WhenOmitted, _) => true,
        (SkipRule::Never, ty) => holds_null(ty, contracts),
        (SkipRule::WhenNone, TypeRef::Option(inner)) => holds_null(inner, contracts),
        (SkipRule::WhenNone, _) => false,
    }
}

/// Whether a value of this type can serialize to `null`.
fn holds_null(ty: &TypeRef, contracts: &Contracts) -> bool {
    holds_null_within(ty, contracts, &mut BTreeSet::new())
}

/// [`holds_null`], remembering the named types already met so a recursive type ends.
///
/// An untagged union writes whichever branch it holds as that branch alone, so it can write
/// `null` when any branch can.
fn holds_null_within(
    ty: &TypeRef,
    contracts: &Contracts,
    followed: &mut BTreeSet<TypeIndex>,
) -> bool {
    match contracts.unaliased(ty) {
        TypeRef::Option(_) | TypeRef::Presence(_) | TypeRef::Unit | TypeRef::Value => true,
        TypeRef::Boxed(inner) => holds_null_within(inner, contracts, followed),
        TypeRef::Named(index) if followed.insert(*index) => {
            match contracts.get(*index).map(TypeContract::kind) {
                Some(ContractKind::Enum { variants, .. }) => variants
                    .iter()
                    .any(|variant| holds_null_within(&variant.ty, contracts, followed)),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The element type of the sequence a repeated member holds, through the wrappers and aliases
/// [`part_within`] looks through on its way to the sequence.
fn element_of<'a>(ty: &'a TypeRef, contracts: &'a Contracts) -> Option<&'a TypeRef> {
    let mut ty = ty;
    loop {
        match contracts.unaliased(ty) {
            TypeRef::Vec(inner) | TypeRef::Array(inner, _) => return Some(inner),
            TypeRef::Option(inner) | TypeRef::Presence(inner) | TypeRef::Boxed(inner) => {
                ty = inner;
            }
            _ => return None,
        }
    }
}

/// The JSON family: `application/json` and the `+json` structured suffix.
pub(super) fn is_json(media_type: &str) -> bool {
    crate::doc::is_json_media_type(media_type)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::preference;
    use crate::api::tests::{model_of, model_with, with_paths};
    use crate::api::{BodyContract, Reading};
    use crate::config::Config;
    use crate::contract::TypeRef;

    /// A body declaring JSON beside multipart, where the preference alone picks JSON.
    fn two_media_type_upload() -> Value {
        with_paths(json!({
            "/dokumente": {
                "post": {
                    "operationId": "uploadDocument",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {"schema": {"type": "object", "properties": {"url": {"type": "string"}}}},
                            "multipart/form-data": {"schema": {"type": "object", "properties": {
                                "file": {"type": "string", "format": "binary"},
                                "anzeigename": {"type": "string"},
                            }}},
                        },
                    },
                    "responses": {"201": {"description": "made"}},
                },
            },
        }))
    }

    fn media_types(entries: &[(&str, &str)]) -> Config {
        Config {
            media_types: entries
                .iter()
                .map(|(position, media_type)| ((*position).to_owned(), (*media_type).to_owned()))
                .collect(),
            ..Config::default()
        }
    }

    /// A member whose type is named is a text part when the name stands for a scalar: a string
    /// enum, or a component that is a scalar; and a file stays a file under `Presence`.
    #[test_util::test]
    fn a_named_scalar_member_is_a_text_part() {
        use crate::api::PartKind::{File, Json, Text};
        let mut document = with_paths(json!({
            "/files": {
                "post": {
                    "operationId": "upload",
                    "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                        "type": "object",
                        "required": ["purpose"],
                        "properties": {
                            "purpose": {"type": "string", "enum": ["assistants", "batch"]},
                            "id": {"$ref": "#/components/schemas/Id"},
                            "modes": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}},
                            "count": {"type": "integer"},
                            "tags": {"$ref": "#/components/schemas/Tags"},
                            "grid": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}},
                            "meta": {"type": "object", "properties": {"k": {"type": "string"}}},
                            "file": {"type": ["string", "null"], "format": "binary"},
                        },
                    }}}},
                    "responses": {"201": {"description": "made"}},
                },
            },
        }));
        document["components"] = json!({"schemas": {
            "Id": {"type": "string", "pattern": "^f"},
            "Tags": {"type": "array", "items": {"type": "string"}},
        }});
        let config = Config {
            preserve_optional_nullable: true,
            ..Config::default()
        };
        let (model, _) = model_with(document, &config)?;
        let Some(BodyContract::Multipart { parts, reading, .. }) = &model.operations()[0].body
        else {
            eyre::bail!("the body is multipart");
        };
        let Reading::Object { members, others } = reading else {
            eyre::bail!("the body reads as an object");
        };
        let part = |wire: &str| {
            let part = parts.iter().find(|part| part.wire_name == wire)?;
            let row = members.iter().find(|member| member.wire_name == wire)?;
            Some((part.kind, part.repeated, row.reading.clone()))
        };
        let items = |row: Reading| Reading::Items(Box::new(row));
        // String-typed parts keep their text, and the rest are re-read; a list of lists has
        // each element written as its JSON, and file and JSON parts keep what the parser built.
        assert_eq!(part("purpose"), Some((Text, false, Reading::Keep)));
        assert_eq!(part("id"), Some((Text, false, Reading::Keep)));
        assert_eq!(part("modes"), Some((Text, true, items(Reading::Keep))));
        assert_eq!(part("count"), Some((Text, false, Reading::Spelled)));
        assert_eq!(part("tags"), Some((Text, true, items(Reading::Keep))));
        assert_eq!(part("grid"), Some((Text, true, items(Reading::Spelled))));
        assert_eq!(part("meta"), Some((Json, false, Reading::Keep)));
        assert_eq!(part("file"), Some((File, false, Reading::Keep)));
        assert_eq!(**others, Reading::Spelled);
    }

    /// A union whose every branch is a string is a string on the wire, so it is a text part, as a
    /// single string member or a string enum is, and the server keeps its text.
    ///
    /// Re-reading that text as JSON would turn a string that spells a number into a number no
    /// branch accepts.
    /// A union that may also hold a number, an object, a list or raw bytes stays one JSON part, and
    /// so does a union of strings whose encoding declares JSON.
    #[test_util::test]
    fn a_union_of_strings_is_a_text_part_that_keeps_its_text() {
        use crate::api::PartKind::{Json, Text};
        let mut document = with_paths(json!({
            "/files": {"post": {
                "operationId": "upload",
                "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "properties": {
                        "subject": {"oneOf": [
                            {"type": "string", "pattern": "^ct_[0-9a-z]+$"},
                            {"type": "string", "const": "submission"},
                        ]},
                        "named": {"$ref": "#/components/schemas/Subject"},
                        "mixed": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                        "subjects": {"type": "array", "items": {"$ref": "#/components/schemas/Subject"}},
                        "shaped": {"oneOf": [
                            {"type": "string"},
                            {"type": "object", "properties": {"id": {"type": "string"}}},
                        ]},
                        "listed": {"oneOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}},
                        ]},
                        "raw": {"oneOf": [
                            {"type": "string", "pattern": "^ct_"},
                            {"type": "string", "format": "binary"},
                        ]},
                        "aliased": {"$ref": "#/components/schemas/Subjects"},
                        "nested": {"type": "array", "items": {"$ref": "#/components/schemas/Subjects"}},
                        "quoted": {"$ref": "#/components/schemas/Subject"},
                        "count": {"type": "integer"},
                    },
                }, "encoding": {"quoted": {"contentType": "application/json"}}}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        document["components"] = json!({"schemas": {
            "Subject": {"oneOf": [
                {"$ref": "#/components/schemas/ContactId"},
                {"$ref": "#/components/schemas/Kind"},
            ]},
            "Subjects": {"type": "array", "items": {"$ref": "#/components/schemas/Subject"}},
            "ContactId": {"type": "string", "pattern": "^ct_"},
            "Kind": {"type": "string", "enum": ["submission", "contact"]},
        }});
        let (model, _) = model_of(document)?;
        let Some(BodyContract::Multipart { parts, reading, .. }) = &model.operations()[0].body
        else {
            eyre::bail!("the body is multipart");
        };
        let Reading::Object { members, .. } = reading else {
            eyre::bail!("the body reads as an object");
        };
        let part = |wire: &str| {
            let part = parts.iter().find(|part| part.wire_name == wire)?;
            let row = members.iter().find(|member| member.wire_name == wire)?;
            Some((
                part.kind,
                part.repeated,
                part.keeps_text,
                row.reading.clone(),
            ))
        };
        let items = |row: Reading| Reading::Items(Box::new(row));

        // Unions of strings, inline and through names, are text parts that keep their text, one
        // per element of a list.
        assert_eq!(part("subject"), Some((Text, false, true, Reading::Keep)));
        assert_eq!(part("named"), Some((Text, false, true, Reading::Keep)));
        assert_eq!(
            part("subjects"),
            Some((Text, true, true, items(Reading::Keep)))
        );
        assert_eq!(
            part("aliased"),
            Some((Text, true, true, items(Reading::Keep)))
        );
        // A list of lists has each inner list written as its JSON text, as a list of lists of
        // strings has.
        assert_eq!(
            part("nested"),
            Some((Text, true, false, items(Reading::Spelled)))
        );
        // A number is a text part re-read as the value its text spells.
        assert_eq!(part("count"), Some((Text, false, false, Reading::Spelled)));
        // A union that may hold a number, an object, a list or raw bytes is one JSON part.
        assert_eq!(part("mixed"), Some((Json, false, false, Reading::Keep)));
        assert_eq!(part("shaped"), Some((Json, false, false, Reading::Keep)));
        assert_eq!(part("listed"), Some((Json, false, false, Reading::Keep)));
        assert_eq!(part("raw"), Some((Json, false, false, Reading::Keep)));
        // A union of strings whose encoding declares JSON stays one JSON part.
        assert_eq!(part("quoted"), Some((Json, false, false, Reading::Keep)));
    }

    /// A string whose encoding declares a JSON content type is a JSON part, so the part holds the
    /// JSON value its header names rather than bare text.
    ///
    /// That holds for every string on the wire, through names and lists, and a list stays one part
    /// per element.
    /// A number or a boolean declared JSON is a JSON part too: its text is already its JSON, and
    /// only a JSON part carries an explicit `null`.
    /// Base64 text encodes the declared content rather than being it, so it stays a text part.
    #[test_util::test]
    fn a_string_whose_encoding_declares_json_is_a_json_part() {
        use crate::api::PartKind::{Json, Text};
        let json = json!({"contentType": "application/json"});
        let mut document = with_paths(json!({
            "/files": {"post": {
                "operationId": "upload",
                "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "properties": {
                        "plain": {"type": "string"},
                        "bare": {"type": "string"},
                        "kind": {"$ref": "#/components/schemas/Kind"},
                        "day": {"type": "string", "format": "date"},
                        "named": {"$ref": "#/components/schemas/Name"},
                        "names": {"type": "array", "items": {"type": "string"}},
                        "nested": {"type": "array", "items": {
                            "type": ["array", "null"], "items": {"$ref": "#/components/schemas/Subject"},
                        }},
                        "count": {"type": "integer"},
                        "flag": {"type": "boolean"},
                        "ratio": {"type": "number"},
                        "encoded": {"type": "string", "contentEncoding": "base64"},
                    },
                }, "encoding": {
                    "plain": json.clone(),
                    "kind": json.clone(),
                    "day": json.clone(),
                    "named": json.clone(),
                    "names": json.clone(),
                    "nested": json.clone(),
                    "count": json.clone(),
                    "flag": json.clone(),
                    "ratio": json.clone(),
                    "encoded": json,
                }}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        document["components"] = json!({"schemas": {
            "Kind": {"type": "string", "enum": ["submission", "contact"]},
            "Name": {"type": "string", "pattern": "^[a-z]+$"},
            "Subject": {"oneOf": [
                {"type": "string", "pattern": "^ct_"},
                {"type": "string", "const": "submission"},
            ]},
        }});
        let (model, _) = model_of(document)?;
        let Some(BodyContract::Multipart { parts, reading, .. }) = &model.operations()[0].body
        else {
            eyre::bail!("the body is multipart");
        };
        let Reading::Object { members, .. } = reading else {
            eyre::bail!("the body reads as an object");
        };
        let part = |wire: &str| {
            let part = parts.iter().find(|part| part.wire_name == wire)?;
            let row = members.iter().find(|member| member.wire_name == wire)?;
            Some((
                part.kind,
                part.repeated,
                part.keeps_text,
                row.reading.clone(),
            ))
        };

        // Strings declared JSON, directly, through names and as a format, are JSON parts the
        // server parses back.
        for wire in ["plain", "kind", "day", "named"] {
            assert_eq!(
                part(wire),
                Some((Json, false, false, Reading::Keep)),
                "{wire}"
            );
        }
        // A list is one JSON part per element, and so is a list of nullable lists of a union of
        // strings, whose `null` element is a JSON `null` rather than the text `null`.
        assert_eq!(part("names"), Some((Json, true, false, Reading::Keep)));
        assert_eq!(part("nested"), Some((Json, true, false, Reading::Keep)));
        // The same string with no encoding is a text part that keeps its text.
        assert_eq!(part("bare"), Some((Text, false, true, Reading::Keep)));
        // Numbers and booleans declared JSON are JSON parts the server parses back.
        for wire in ["count", "flag", "ratio"] {
            assert_eq!(
                part(wire),
                Some((Json, false, false, Reading::Keep)),
                "{wire}"
            );
        }
        // Base64 text keeps its bytes.
        assert_eq!(part("encoded"), Some((Text, false, true, Reading::Keep)));
    }

    /// Every member the `multipart-null` records of a model stand for, sorted.
    ///
    /// The class folds per document, so a record names its first member as its location and the
    /// rest as related locations; a member reported twice appears twice.
    fn multipart_nulls(diagnostics: &[crate::Diagnostic]) -> Vec<String> {
        let mut members: Vec<String> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.class() == crate::BreakageClass::MultipartNull)
            .flat_map(|diagnostic| {
                std::iter::once(diagnostic.location())
                    .chain(diagnostic.related())
                    .map(ToString::to_string)
            })
            .collect();
        members.sort();
        members
    }

    /// Every member whose explicit `null` the writer sends as absent is reported once, and a
    /// member whose `null` reaches the wire is not.
    ///
    /// The writer carries a single JSON part's `null` and a repeated JSON member's `null` elements,
    /// and drops a text or file part's `null` and a repeated member's `null` as a whole.
    #[test_util::test]
    fn a_null_a_multipart_part_cannot_carry_is_reported() {
        let json = json!({"contentType": "application/json"});
        let document = with_paths(json!({
            "/files": {"post": {
                "operationId": "upload",
                "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "required": ["note", "file", "list", "meta", "count"],
                    "properties": {
                        // Reported: a text or file part that can be `null`
                        "note": {"type": ["string", "null"]},
                        "kept": {"type": ["string", "null"]},
                        "file": {"type": ["string", "null"], "format": "binary"},
                        // Reported: `null` elements of a text list, and a `null` list
                        "tags": {"type": "array", "items": {"type": ["string", "null"]}},
                        "list": {"type": ["array", "null"], "items": {
                            "type": "object", "properties": {"k": {"type": "string"}},
                        }},
                        // Not reported: JSON parts carry `null`, and the rest never write one
                        "meta": {"type": ["object", "null"], "properties": {"k": {"type": "string"}}},
                        "count": {"type": ["integer", "null"]},
                        "ids": {"type": "array", "items": {"type": ["integer", "null"]}},
                        "plain": {"type": "string"},
                    },
                }, "encoding": {"count": json.clone(), "ids": json}}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        // `kept` is optional, so it can hold an explicit `null` only while presence is preserved.
        let config = Config {
            preserve_optional_nullable: true,
            ..Config::default()
        };
        let (_, diagnostics) = model_with(document, &config)?;

        let schema = "/paths/~1files/post/requestBody/content/multipart~1form-data/schema";
        let reported = multipart_nulls(&diagnostics);
        let expected: Vec<String> = ["file", "kept", "list", "note", "tags"]
            .iter()
            .map(|name| format!("{schema}/properties/{name}"))
            .collect();
        assert_eq!(reported, expected);
    }

    /// A member is a schema property, so two operations sending one component report its member
    /// once, while two schemas that merely share a field name are two members.
    #[test_util::test]
    fn a_multipart_member_is_reported_once_whichever_operations_send_it() {
        let shared = |id: &str| {
            json!({"post": {
                "operationId": id,
                "requestBody": {"content": {"multipart/form-data": {
                    "schema": {"$ref": "#/components/schemas/Note"},
                }}},
                "responses": {"204": {"description": "done"}},
            }})
        };
        let inline = json!({"post": {
            "operationId": "third",
            "requestBody": {"content": {"multipart/form-data": {"schema": {
                "type": "object",
                "required": ["note"],
                "properties": {
                    "note": {"type": ["string", "null"]},
                    "title": {"type": "string"},
                },
            }}}},
            "responses": {"204": {"description": "done"}},
        }});
        let mut document = with_paths(json!({
            "/first": shared("first"),
            "/second": shared("second"),
            "/third": inline,
        }));
        document["components"] = json!({"schemas": {"Note": {
            "type": "object",
            "required": ["note"],
            "properties": {"note": {"type": ["string", "null"]}},
        }}});
        let (_, diagnostics) = model_of(document)?;

        // The shared component's member once, and the inline schema's member of the same name
        // on its own: the inline schema has a member `Note` lacks, so it is another type rather
        // than the same one met again.
        let reported = multipart_nulls(&diagnostics);
        assert_eq!(
            reported,
            [
                "/components/schemas/Note/properties/note",
                "/paths/~1third/post/requestBody/content/multipart~1form-data/schema/properties/note",
            ]
        );
    }

    /// Equal inline schemas share one type whose origin is the first of them, so a member is
    /// reported at the schema of the operation that loses its `null`, not at the one whose
    /// encoding carries it.
    #[test_util::test]
    fn a_lost_null_is_reported_at_the_schema_of_the_operation_that_loses_it() {
        let operation = |id: &str, encoding: Value| {
            json!({"post": {
                "operationId": id,
                "requestBody": {"content": {"multipart/form-data": {
                    "schema": {
                        "type": "object",
                        "required": ["note"],
                        "properties": {"note": {"type": ["string", "null"]}},
                    },
                    "encoding": encoding,
                }}},
                "responses": {"204": {"description": "done"}},
            }})
        };
        let document = with_paths(json!({
            "/a": operation("first", json!({"note": {"contentType": "application/json"}})),
            "/b": operation("second", json!({})),
        }));
        let (_, diagnostics) = model_of(document)?;

        assert_eq!(
            multipart_nulls(&diagnostics),
            ["/paths/~1b/post/requestBody/content/multipart~1form-data/schema/properties/note"]
        );
    }

    /// One member sent by two operations whose encodings lose its `null` in different ways is
    /// reported for each way, whichever operation comes first.
    #[test_util::test]
    fn each_way_a_shared_member_loses_its_null_is_reported() {
        let operation = |id: &str, encoding: Value| {
            json!({"post": {
                "operationId": id,
                "requestBody": {"content": {"multipart/form-data": {
                    "schema": {"$ref": "#/components/schemas/Tags"},
                    "encoding": encoding,
                }}},
                "responses": {"204": {"description": "done"}},
            }})
        };
        let mut document = with_paths(json!({
            "/a": operation("first", json!({"tags": {"contentType": "application/json"}})),
            "/b": operation("second", json!({})),
        }));
        document["components"] = json!({"schemas": {"Tags": {
            "type": "object",
            "required": ["tags"],
            "properties": {"tags": {
                "type": ["array", "null"],
                "items": {"type": ["string", "null"]},
            }},
        }}});
        let (_, diagnostics) = model_of(document)?;

        // The JSON-encoded list keeps its `null` elements and loses only a `null` list; the text
        // list loses both, a different loss at the same member.
        let details: std::collections::BTreeSet<&str> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.class() == crate::BreakageClass::MultipartNull)
            .map(crate::Diagnostic::detail)
            .collect();
        assert_eq!(details.len(), 2, "{details:?}");
        assert_eq!(
            multipart_nulls(&diagnostics),
            [
                "/components/schemas/Tags/properties/tags",
                "/components/schemas/Tags/properties/tags",
            ]
        );
    }

    /// A member no schema of this body declares directly, here behind an `anyOf` two operations
    /// share one type for, is reported at this operation's own body, naming the member: each such
    /// member on its own.
    #[test_util::test]
    fn a_member_without_a_declaration_of_its_own_is_reported_at_its_body() {
        let operation = |id: &str, encoding: Value| {
            json!({"post": {
                "operationId": id,
                "requestBody": {"content": {"multipart/form-data": {
                    "schema": {"anyOf": [
                        {"type": "object", "required": ["note", "title"], "properties": {
                            "note": {"type": ["string", "null"]},
                            "title": {"type": ["string", "null"]},
                        }},
                        {"type": "null"},
                    ]},
                    "encoding": encoding,
                }}},
                "responses": {"204": {"description": "done"}},
            }})
        };
        let document = with_paths(json!({
            "/a": operation("first", json!({
                "note": {"contentType": "application/json"},
                "title": {"contentType": "application/json"},
            })),
            "/b": operation("second", json!({})),
        }));
        let (_, diagnostics) = model_of(document)?;

        let mut records: Vec<(String, &str)> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.class() == crate::BreakageClass::MultipartNull)
            .map(|diagnostic| (diagnostic.location().to_string(), diagnostic.detail()))
            .collect();
        records.sort();
        let body = "/paths/~1b/post/requestBody/content/multipart~1form-data/schema";
        let [(first, note), (second, title)] = records.as_slice() else {
            eyre::bail!("one record per member of `/b` alone: {records:?}");
        };
        assert_eq!((first.as_str(), second.as_str()), (body, body));
        assert!(note.contains("`note`"), "{note}");
        assert!(title.contains("`title`"), "{title}");
    }

    /// A referenced request body reports at the schema where it is written, a pointer that exists.
    #[test_util::test]
    fn a_referenced_request_body_reports_where_its_schema_is_written() {
        let mut document = with_paths(json!({
            "/upload": {"post": {
                "operationId": "upload",
                "requestBody": {"$ref": "#/components/requestBodies/Upload"},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        document["components"] = json!({"requestBodies": {"Upload": {"content": {
            "multipart/form-data": {"schema": {"anyOf": [
                {"type": "object", "required": ["note"],
                 "properties": {"note": {"type": ["string", "null"]}}},
                {"type": "null"},
            ]}},
        }}}});
        let (_, diagnostics) = model_of(document)?;

        let reported = multipart_nulls(&diagnostics);
        assert_eq!(
            reported,
            ["/components/requestBodies/Upload/content/multipart~1form-data/schema"]
        );
    }

    /// A `$ref` beside the `allOf` does not hide the branches: each member is still reported where
    /// it is declared.
    #[test_util::test]
    fn a_reference_beside_all_of_keeps_each_member_at_its_branch() {
        let mut document = with_paths(json!({
            "/files": {"post": {
                "operationId": "upload",
                "requestBody": {"content": {"multipart/form-data": {"schema": {
                    "$ref": "#/components/schemas/Base",
                    "allOf": [
                        {"type": "object", "required": ["left"],
                         "properties": {"left": {"type": ["string", "null"]}}},
                        {"type": "object", "required": ["right"],
                         "properties": {"right": {"type": ["string", "null"]}}},
                    ],
                }}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        document["components"] = json!({"schemas": {"Base": {
            "type": "object", "properties": {"id": {"type": "string"}},
        }}});
        let (_, diagnostics) = model_of(document)?;

        let schema = "/paths/~1files/post/requestBody/content/multipart~1form-data/schema";
        assert_eq!(
            multipart_nulls(&diagnostics),
            [
                format!("{schema}/allOf/0/properties/left"),
                format!("{schema}/allOf/1/properties/right"),
            ]
        );
    }

    /// A union of strings is a text part, and a nullable branch makes it able to hold `null`.
    #[test_util::test]
    fn a_text_union_with_a_nullable_branch_reports_its_lost_null() {
        let document = with_paths(json!({
            "/files": {"post": {
                "operationId": "upload",
                "requestBody": {"content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "required": ["owner"],
                    "properties": {"owner": {"anyOf": [
                        {"type": ["string", "null"], "format": "uuid"},
                        {"const": "me"},
                    ]}},
                }}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        let (_, diagnostics) = model_of(document)?;

        assert_eq!(
            multipart_nulls(&diagnostics),
            [
                "/paths/~1files/post/requestBody/content/multipart~1form-data/schema/properties/owner"
            ]
        );
    }

    /// A recursive member meets its own type again on the way to its part kind, and is one JSON
    /// part: a nullable wrapper around itself, and a list of itself, have no faithful text form.
    #[test_util::test]
    fn a_recursive_multipart_member_is_one_json_part() {
        use crate::api::PartKind::Json;
        let mut document = with_paths(json!({
            "/x": {"post": {
                "operationId": "postX",
                "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                    "type": "object",
                    "properties": {
                        "maybe": {"$ref": "#/components/schemas/A"},
                        "tree": {"$ref": "#/components/schemas/Tree"},
                    },
                }}}},
                "responses": {"204": {"description": "done"}},
            }},
        }));
        document["components"] = json!({"schemas": {
            "A": {"oneOf": [{"$ref": "#/components/schemas/B"}, {"type": "null"}]},
            "B": {"oneOf": [{"$ref": "#/components/schemas/A"}]},
            "Tree": {"type": "array", "items": {"$ref": "#/components/schemas/Tree"}},
        }});
        let (model, _) = model_of(document)?;
        let Some(BodyContract::Multipart { parts, .. }) = &model.operations()[0].body else {
            eyre::bail!("the body is multipart");
        };
        let part = |wire: &str| {
            parts
                .iter()
                .find(|part| part.wire_name == wire)
                .map(|part| (part.kind, part.repeated))
        };
        assert_eq!(part("maybe"), Some((Json, false)));
        assert_eq!(part("tree"), Some((Json, false)));
    }

    /// The field that captures `additionalProperties` has no row in either body's table, so its
    /// empty wire name describes no member, not even one a client named `""`.
    #[test_util::test]
    fn captured_members_have_no_row() {
        let schema = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "additionalProperties": {"type": "array", "items": {"type": "string"}},
        });
        let (model, _) = model_of(with_paths(json!({
            "/files": {
                "post": {
                    "operationId": "upload",
                    "requestBody": {"required": true, "content": {
                        "multipart/form-data": {"schema": schema},
                    }},
                    "responses": {"201": {"description": "made"}},
                },
            },
            "/forms": {
                "post": {
                    "operationId": "submit",
                    "requestBody": {"required": true, "content": {
                        "application/x-www-form-urlencoded": {"schema": schema},
                    }},
                    "responses": {"201": {"description": "made"}},
                },
            },
        })))?;
        let mut tables = Vec::new();
        for operation in model.operations() {
            let names: Vec<&str> = match &operation.body {
                Some(BodyContract::Multipart { parts, .. }) => {
                    parts.iter().map(|part| part.wire_name.as_str()).collect()
                }
                Some(BodyContract::Form { specs, .. }) => {
                    specs.iter().map(|spec| spec.wire_name.as_str()).collect()
                }
                _ => eyre::bail!("every operation has a multipart or form body"),
            };
            tables.push(names);
        }
        // One multipart table and one form table, each describing only the declared member.
        assert_eq!(tables, [["count"], ["count"]]);
    }

    #[test_util::test]
    fn a_file_part_is_exactly_an_upload_typed_member() {
        // The server-side parser re-wraps `File` parts into the upload object form, which only
        // `support::Upload`'s `Deserialize` reads — so a `File` spec over a `String`-typed
        // member (a base64 `format: byte` member here) would make the generated server reject
        // every request its own client builds.
        let (model, _) = model_of(with_paths(json!({
            "/upload": {
                "post": {
                    "operationId": "upload",
                    "requestBody": {"required": true, "content": {"multipart/form-data": {"schema": {
                        "type": "object",
                        "properties": {
                            "file": {"type": "string", "format": "binary"},
                            "checksum": {"type": "string", "contentEncoding": "base64"},
                            "note": {"type": "string"},
                        },
                    }}}},
                    "responses": {"201": {"description": "made"}},
                },
            },
        })))?;
        let Some(BodyContract::Multipart { parts, .. }) = &model.operations()[0].body else {
            panic!("the body is multipart");
        };
        let kind = |wire: &str| {
            parts
                .iter()
                .find(|part| part.wire_name == wire)
                .map(|part| part.kind)
        };
        assert_eq!(kind("file"), Some(crate::api::PartKind::File));
        assert_eq!(kind("checksum"), Some(crate::api::PartKind::Text));
        assert_eq!(kind("note"), Some(crate::api::PartKind::Text));
    }

    #[test_util::test]
    fn every_declared_request_media_type_becomes_its_own_method() -> eyre::Result<()> {
        let (model, diagnostics) = model_of(two_media_type_upload())?;
        // The preference names JSON as the primary; multipart is the suffixed sibling, sharing
        // the route and serving only the client.
        let names: Vec<&str> = model
            .operations()
            .iter()
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(names, ["upload_document", "upload_document_multipart"]);
        assert!(matches!(
            model.operations()[0].body,
            Some(BodyContract::Json { .. })
        ));
        assert!(matches!(
            model.operations()[1].body,
            Some(BodyContract::Multipart { .. })
        ));
        assert!(!model.operations()[0].body_variant);
        assert!(model.operations()[0].registrable.is_some());
        assert!(model.operations()[1].body_variant);
        assert!(model.operations()[1].registrable.is_none());
        let reported: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        assert!(
            reported
                .iter()
                .any(|line| line.contains("the generated server accepts only `application/json`")),
            "{reported:?}"
        );
        Ok(())
    }

    #[test_util::test]
    fn a_configured_media_type_overrides_the_preference() -> eyre::Result<()> {
        let (model, diagnostics) = model_with(
            two_media_type_upload(),
            &media_types(&[(
                "/paths/~1dokumente/post/requestBody/content",
                "multipart/form-data",
            )]),
        )?;
        // The configured pick decides which media type keeps the operation's own name — and which
        // one the generated server accepts; JSON becomes the suffixed sibling.
        let names: Vec<&str> = model
            .operations()
            .iter()
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(names, ["upload_document", "upload_document_json"]);
        assert!(matches!(
            model.operations()[0].body,
            Some(BodyContract::Multipart { .. })
        ));
        assert!(matches!(
            model.operations()[1].body,
            Some(BodyContract::Json { .. })
        ));
        let reported: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        assert!(
            reported.iter().any(
                |line| line.contains("the generated server accepts only `multipart/form-data`")
            ),
            "{reported:?}"
        );
        Ok(())
    }

    #[test_util::test]
    fn a_wildcard_alternate_is_reported_and_not_generated() -> eyre::Result<()> {
        // `jellyfin`, in miniature: the wildcard says something about the server and is never a
        // content type a client can send, so it stays out of the method list.
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "post": {
                    "operationId": "createPet",
                    "requestBody": {"content": {
                        "application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}},
                        "application/*+json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}},
                    }},
                    "responses": {"201": {"description": "made"}},
                },
            },
        })))?;
        let names: Vec<&str> = model
            .operations()
            .iter()
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(names, ["create_pet"]);
        let reported: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        assert!(
            reported
                .iter()
                .any(|line| line.contains("`application/*+json` is a wildcard")),
            "{reported:?}"
        );
        Ok(())
    }

    #[test_util::test]
    fn a_configured_media_type_may_restate_the_preference() -> eyre::Result<()> {
        let (model, _) = model_with(
            two_media_type_upload(),
            &media_types(&[(
                "/paths/~1dokumente/post/requestBody/content",
                "application/json",
            )]),
        )?;
        assert!(matches!(
            model.operations()[0].body,
            Some(BodyContract::Json { .. })
        ));
        Ok(())
    }

    #[test_util::test]
    fn a_configured_media_type_the_position_does_not_declare_is_rejected() {
        let Err(error) = model_with(
            two_media_type_upload(),
            &media_types(&[(
                "/paths/~1dokumente/post/requestBody/content",
                "application/xml",
            )]),
        ) else {
            panic!("an unhonourable pick must stop generation");
        };
        let message = error.to_string();
        assert!(message.contains("declares only"), "{message}");
        assert!(message.contains("`application/json`"), "{message}");
        assert!(message.contains("`multipart/form-data`"), "{message}");
    }

    #[test_util::test]
    fn a_configured_media_type_at_a_position_without_content_is_rejected() {
        let Err(error) = model_with(
            two_media_type_upload(),
            &media_types(&[(
                "/paths/~1dokumente/get/requestBody/content",
                "application/json",
            )]),
        ) else {
            panic!("a pointer no position matches must stop generation");
        };
        let message = error.to_string();
        assert!(
            message.contains("no generated body or response has content there"),
            "{message}"
        );
    }

    #[test_util::test]
    fn the_json_family_is_preferred_over_everything_else() {
        assert!(preference("application/json") < preference("text/plain"));
        assert!(preference("application/vnd.api+json") < preference("multipart/form-data"));
        assert_eq!(
            preference("application/json; charset=utf-8"),
            preference("application/json")
        );
        // The concrete type beats the wildcard that covers it, and a wildcard loses to everything:
        // `*` is a thing a document may say and never a thing a client may send.
        assert!(preference("application/json") < preference("application/*+json"));
        assert!(preference("text/plain") < preference("application/*+json"));
        assert!(preference("audio/mpeg") < preference("audio/*"));
    }

    #[test_util::test]
    fn a_json_body_is_typed_and_a_binary_one_is_bytes() {
        let (model, _) = model_of(with_paths(json!({
            "/upload": {
                "post": {
                    "operationId": "upload",
                    "requestBody": {
                        "required": true,
                        "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}},
                    },
                    "responses": {"200": {"description": "ok"}},
                },
            },
            "/pets": {
                "post": {
                    "operationId": "createPet",
                    "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}}}},
                    "responses": {"201": {"description": "made"}},
                },
            },
        })))?;
        let upload = model
            .operations()
            .iter()
            .find(|operation| operation.rust_name.as_str() == "upload")
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(matches!(
            upload.body,
            Some(BodyContract::Bytes { required: true, .. })
        ));
        let create = model
            .operations()
            .iter()
            .find(|operation| operation.rust_name.as_str() == "create_pet")
            .ok_or_eyre("test fixture should contain this value")?;
        assert!(matches!(
            create.body,
            Some(BodyContract::Json {
                ty: TypeRef::Named(_),
                required: false,
                ..
            })
        ));
    }
}
