//! The one body an operation sends: which media type, and what contract it becomes.

use std::collections::BTreeMap;

use super::Build;
use super::param::param_shape;
use crate::api::style::{self, ParamShape};
use crate::api::{BodyContract, FormSpec, PartKind, PartSpec, Style};
use crate::contract::{ContractKind, Format, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MediaType, Operation};

impl Build<'_> {
    /// The one body the operation sends.
    pub(super) fn body(
        &self,
        operation: &Operation,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<BodyContract> {
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
            return Some(BodyContract::Json {
                ty: TypeRef::Value,
                required: false,
            });
        };
        let required = body.required.unwrap_or(false);
        let content = body.content.as_ref()?;
        let (media_type, entry) = Self::select(content, &at, ctx)?;
        Some(self.body_of(media_type, entry, required, &at, ctx))
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
            return BodyContract::Json { ty, required };
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
                    BodyContract::Json { ty, required }
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

    /// Which media type an operation's body uses, by fixed preference.
    ///
    /// The alternates are reported rather than silently dropped, because a caller who needs one is
    /// entitled to know progeny saw it and chose another.
    pub(super) fn select<'a>(
        content: &'a BTreeMap<String, MediaType>,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<(&'a str, &'a MediaType)> {
        let chosen = content
            .keys()
            .min_by_key(|media_type| (preference(media_type), media_type.as_str()))?;
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
/// The type layer renders `format: binary` as `String` — inside a JSON payload a binary property
/// *is* a string, and the type layer has no position to tell it otherwise. Here the position
/// exists, so the string's bytes are what the part carries. The consequence, stated because it is a
/// real limitation rather than an oversight: a part whose content is not valid UTF-8 cannot be
/// constructed, because the field that holds it is a `String`. Lifting that would mean the type
/// depending on the position, which would fork a component type shared with a JSON body.
fn part_of(ty: &TypeRef) -> (PartKind, bool) {
    match ty {
        TypeRef::Format(Format::Binary | Format::Base64) => (PartKind::File, false),
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
    use serde_json::json;

    use super::preference;
    use crate::api::BodyContract;
    use crate::api::tests::{model_of, with_paths};
    use crate::contract::TypeRef;

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
                required: false
            })
        ));
    }
}
