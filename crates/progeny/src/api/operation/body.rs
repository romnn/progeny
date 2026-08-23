//! The one body an operation sends: which media type, and what contract it becomes.

use std::collections::BTreeMap;

use super::Build;
use super::param::param_shape;
use crate::api::style::{self, ParamShape};
use crate::api::{BodyContract, FormSpec, PartKind, PartSpec, Style};
use crate::contract::{ContractKind, Format, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MediaType, Operation};

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
        &self,
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
            };
        }
        match base.as_str() {
            "application/x-www-form-urlencoded" => BodyContract::Form {
                specs: self.form_specs(&ty, entry, at, ctx),
                ty,
                required,
            },
            "multipart/form-data" => BodyContract::Multipart {
                parts: self.part_specs(&ty, entry),
                ty,
                required,
            },
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
                    }
                } else {
                    BodyContract::Bytes {
                        content_type: chosen.to_owned(),
                        required,
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
            for field in fields {
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
            });
        }
        specs
    }

    /// What the document was specific about, member by member, for a multipart body.
    ///
    /// Read from the *contract* rather than from the schema: the contract is the frozen record of
    /// what the type actually carries, and a part list derived from anything else could describe a
    /// member the emitted type does not have.
    fn part_specs(&self, ty: &TypeRef, entry: &MediaType) -> Vec<PartSpec> {
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
        fields
            .iter()
            .map(|field| {
                let (kind, repeated) = part_of(&field.ty);
                PartSpec {
                    kind,
                    repeated,
                    // A `contentType` naming several types — `cloudflare` writes ten of them separated
                    // by commas — is a list of what the server accepts, not an instruction about what
                    // to send. Only a single type is a decision.
                    content_type: declared
                        .get(field.wire_name.as_str())
                        .filter(|declared| !declared.contains(','))
                        .map(|declared| (*declared).to_owned()),
                    wire_name: field.wire_name.clone(),
                }
            })
            .collect()
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
/// the type layer left as `String` — a base64 (`format: byte`) member, or a schema the
/// encoding-collapse rule kept on the JSON spelling — is a text part whose bytes are the
/// string's, with the stated consequence that such a part cannot carry invalid UTF-8.
fn part_of(ty: &TypeRef) -> (PartKind, bool) {
    match ty {
        TypeRef::Upload => (PartKind::File, false),
        // An option says nothing about the shape on the wire; a box says nothing at all.
        TypeRef::Option(inner) | TypeRef::Boxed(inner) => part_of(inner),
        // The one wrapper that does say something: each element is its own part.
        TypeRef::Vec(inner) | TypeRef::Array(inner, _) => (part_of(inner).0, true),
        TypeRef::Bool
        | TypeRef::I64
        | TypeRef::U64
        | TypeRef::F64
        | TypeRef::String
        | TypeRef::Format(_) => (PartKind::Text, false),
        // A named type, a map, a tuple or a degraded `Value` is structured, and structured members
        // have no faithful text form.
        _ => (PartKind::Json, false),
    }
}

/// The JSON family: `application/json` and the `+json` structured suffix.
pub(super) fn is_json(media_type: &str) -> bool {
    let base = media_type.split(';').next().unwrap_or(media_type).trim();
    base == "application/json" || base.ends_with("+json")
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::preference;
    use crate::api::BodyContract;
    use crate::api::tests::{model_of, model_with, with_paths};
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
        assert!(message.contains("declares no content there"), "{message}");
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
