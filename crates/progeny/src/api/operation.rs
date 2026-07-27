//! Walking the document's paths into operations.
//!
//! Three tolerance rules govern everything here, and they are the same rule seen from three
//! distances:
//!
//! * **A position degrades before an operation does.** A response body progeny cannot type becomes
//!   `serde_json::Value`; the operation stays. `miro` writes 612 references to a
//!   `components.responses` section it never declares, and skipping an operation per reference
//!   would delete most of that API over one missing member.
//! * **An operation degrades before it disappears.** An optional parameter with no defined
//!   serialization is omitted, because a caller who never sets it is unaffected.
//! * **An operation disappears only when calling it is impossible.** A *required* parameter with no
//!   defined serialization, or a path variable no parameter declares, leaves nothing that could be
//!   sent — and a method that cannot build its own request is worse than a method that is absent.

use std::collections::BTreeMap;

use super::Style;
use super::route::{self, PathTemplate};
use super::style::{self, Location, ParamShape};
use super::{
    ApiModel, BodyContract, FormSpec, Method, OperationContract, ParamContract, PartKind, PartSpec,
    ResponseArm, ResponseContract, StatusPattern,
};
use crate::config::Config;
use crate::contract::{ContractKind, Contracts, Format, Namer, RustIdent, TypeRef};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{MaybeRef, MediaType, Operation, Parameter, PathItem, Response};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;
use crate::shape::Docs;

/// Every operation the document declares, named and lowered.
pub(super) fn run(
    resolved: &ResolvedDocument,
    contracts: &Contracts,
    _config: &Config,
    ctx: &mut Ctx,
) -> ApiModel {
    let mut build = Build {
        resolved,
        contracts,
        namer: Namer::default(),
    };
    let mut operations = Vec::new();
    // Webhooks are deliberately absent: they are carried losslessly in the document model and are
    // not rendered in v1, and an operation the caller cannot call has no business in the catalogue
    // of operations the caller can call.
    let paths = resolved
        .document()
        .paths
        .as_ref()
        .map(|paths| &paths.items)
        .into_iter()
        .flatten();
    for (route, item) in paths {
        let Some(item) = resolved.path_item(item) else {
            continue;
        };
        build.path_item(item, route, &mut operations, ctx);
    }

    super::registrable::classify(&mut operations, ctx);
    ApiModel {
        operations,
        servers: Vec::new(),
    }
}

struct Build<'a> {
    resolved: &'a ResolvedDocument,
    contracts: &'a Contracts,
    /// Method names, kept unique across the whole client.
    namer: Namer,
}

impl Build<'_> {
    fn path_item(
        &mut self,
        item: &PathItem,
        route: &str,
        out: &mut Vec<OperationContract>,
        ctx: &mut Ctx,
    ) {
        let at = JsonPointer::root().child("paths").child(route);
        let template = match route::parse(route) {
            Ok(template) => template,
            Err(malformed) => {
                ctx.report(Diagnostic::new(
                    BreakageClass::UnregistrableRoute,
                    Action::Degrade,
                    at,
                    format!(
                        "{}; every operation on this path is skipped, because a request to it \
                         cannot be addressed",
                        malformed.detail
                    ),
                ));
                return;
            }
        };

        for (method, operation) in methods(item) {
            let Some(operation) = operation else {
                continue;
            };
            let at = at.child(method.slug());
            if let Some(built) = self.operation(operation, item, method, &template, route, &at, ctx)
            {
                out.push(built);
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an operation is the product of its document node, the path item above it, its \
                  method, its route and where it sits; threading them through a struct would put \
                  the same values one indirection further away"
    )]
    fn operation(
        &mut self,
        operation: &Operation,
        item: &PathItem,
        method: Method,
        template: &PathTemplate,
        route: &str,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<OperationContract> {
        let params = self.params(operation, item, at, ctx)?;

        // A template variable nothing declares leaves a hole in the URL. Checked after the
        // parameters, because path-level parameters count and only the merge knows all of them.
        let declared: Vec<String> = params
            .iter()
            .filter(|param| param.style.location() == Location::Path)
            .map(|param| param.wire_name.clone())
            .collect();
        let unbound = route::unbound(template, &declared);
        if !unbound.is_empty() {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                at.clone(),
                format!(
                    "the path names {} that no path parameter declares, so the URL cannot be \
                     built; the operation is skipped",
                    unbound
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            ));
            return None;
        }

        let rust_name = self.name(operation, method, route, at, ctx);
        // Body first, then responses: the order the struct literal used to evaluate them in, kept
        // so that hoisting the calls does not reorder their diagnostics under the fold cap.
        let body = self.body(operation, at, ctx);
        let mut responses = self.responses(operation, at, ctx);
        if method == Method::Head {
            // A `HEAD` response has no body on the wire — the transport strips it, whatever the
            // handler wrote — so the schemas these arms declare document the `GET` twin, not
            // anything this operation can receive. Decoding them is how the wire probe found this:
            // every `HEAD` in `jellyfin` failed on a body that HTTP guarantees is absent. The arms
            // and their statuses stay; only the payload type becomes what a bodiless response
            // actually carries.
            for arm in responses.arms.iter_mut().chain(responses.default.as_mut()) {
                arm.ty = crate::contract::TypeRef::Unit;
            }
        }
        Some(OperationContract {
            rust_name,
            method,
            path: template.clone(),
            params,
            body,
            responses,
            docs: docs_of(operation),
            // Filled in once every operation exists: whether a route collides is a question about
            // the whole set, so it cannot be answered while the set is still being built.
            registrable: None,
            pagination: None,
            origin: at.clone(),
        })
    }

    /// The method name, kept unique across the client.
    ///
    /// An `operationId` when there is one, because that is the name the document chose and a
    /// reader of both will look for it. Otherwise the method and the route, which is deterministic
    /// and says where the call goes.
    fn name(
        &mut self,
        operation: &Operation,
        method: Method,
        route: &str,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> RustIdent {
        let declared = operation
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let wanted = if let Some(id) = declared {
            RustIdent::method(std::slice::from_ref(&id.to_owned()))
        } else {
            {
                let mut segments = vec![method.slug().to_owned()];
                segments.extend(
                    route
                        .split('/')
                        .filter(|segment| !segment.is_empty())
                        .map(|segment| segment.trim_matches(['{', '}']).to_owned()),
                );
                let synthesized = RustIdent::method(&segments);
                // The sentence names no identifier on purpose: a detail that varies per occurrence
                // cannot be aggregated, and most documents that omit one `operationId` omit every
                // one of them. The count is the finding; the names are in the generated source.
                ctx.report(Diagnostic::new(
                    BreakageClass::CollidingOperationId,
                    Action::Repair,
                    at.clone(),
                    "the operation declares no `operationId`, so its method is named after its \
                     method and path; adding one to the document pins the name",
                ));
                synthesized
            }
        };

        let unique = self.namer.unique(wanted.clone());
        if unique != wanted {
            ctx.report(Diagnostic::new(
                BreakageClass::CollidingOperationId,
                Action::Repair,
                at.clone(),
                format!(
                    "another operation is already called `{wanted}`, so this one is called \
                     `{unique}`; the two are told apart by a suffix because nothing in the \
                     document tells them apart"
                ),
            ));
        }
        unique
    }

    /// The operation's parameters, path-level ones included.
    ///
    /// Returns `None` when the operation cannot be called at all.
    fn params(
        &mut self,
        operation: &Operation,
        item: &PathItem,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> Option<Vec<ParamContract>> {
        // An operation-level parameter overrides a path-level one with the same (name, in), which
        // is the specification's own rule; a map keyed by the pair implements it exactly.
        let mut merged: BTreeMap<(String, String), &Parameter> = BTreeMap::new();
        let listed = item
            .parameters
            .iter()
            .flatten()
            .chain(operation.parameters.iter().flatten());
        for node in listed {
            let Some(parameter) = self.resolved.parameter(node) else {
                continue;
            };
            let name = parameter.name.clone().unwrap_or_default();
            let location = parameter.location.clone().unwrap_or_default();
            merged.insert((name, location), parameter);
        }

        let mut used = Namer::default();
        // The builder interface reserves these, so a parameter that wants one has to take a
        // suffix. `client` and `body` are fields of every builder and `new`/`send` are its methods;
        // `jellyfin` declares a query parameter called `client`, and without this the generated
        // struct declares the field twice and does not compile. Reserved here rather than in the
        // renderer because which names the interface occupies is a fact about the interface, and a
        // renderer that renamed a parameter would be a renderer making a decision.
        for reserved in ["client", "body", "new", "send"] {
            let _ = used.unique(RustIdent::field(reserved));
        }
        let mut params = Vec::new();
        for ((name, _), parameter) in merged {
            let required = parameter.required.unwrap_or(false);
            let (ty, shape) = self.param_type(parameter);
            let style = match style::classify(parameter, shape) {
                Ok(style) => style,
                Err(undefined) => {
                    // A path parameter is required whatever the document says: the URL has a hole
                    // in it either way.
                    let load_bearing = required || parameter.location.as_deref() == Some("path");
                    ctx.report(Diagnostic::new(
                        BreakageClass::QuerySerializationStyle,
                        Action::Degrade,
                        at.child("parameters").child(name.clone()),
                        if load_bearing {
                            format!(
                                "{}; the parameter is required, so the operation is skipped rather \
                                 than called without it",
                                undefined.detail
                            )
                        } else {
                            format!(
                                "{}; the parameter is optional, so it is left out and the rest of \
                                 the operation is generated",
                                undefined.detail
                            )
                        },
                    ));
                    if load_bearing {
                        return None;
                    }
                    continue;
                }
            };
            params.push(ParamContract {
                rust_name: used.unique(RustIdent::field(&name)),
                wire_name: name,
                style,
                required,
                ty,
                docs: Docs {
                    description: parameter.description.clone(),
                    deprecated: parameter.deprecated.unwrap_or(false),
                    ..Docs::default()
                },
            });
        }
        Some(params)
    }

    /// A parameter's type, and which of the three serialization shapes it has.
    fn param_type(&self, parameter: &Parameter) -> (TypeRef, ParamShape) {
        // `content` on a parameter means "this value is a document in that media type", which is
        // one media type's worth of encoding rather than a style. Typed as its schema and treated
        // as a primitive, because what goes in the URL is the encoded text.
        let id = parameter.schema.or_else(|| {
            parameter
                .content
                .as_ref()
                .and_then(|content| content.values().find_map(|entry| entry.schema))
        });
        let Some(id) = id else {
            return (TypeRef::String, ParamShape::Primitive);
        };
        let key = crate::shape::key_of(self.resolved, id);
        let ty = self
            .contracts
            .type_of(&key)
            .cloned()
            .unwrap_or(TypeRef::String);
        // Derived from the *type the extraction decodes into*, not from the classified shape. The
        // two disagreed for orb's `status[]`: a nullable array classifies as a union, which is not
        // `Shape::Array`, so the shape said "primitive" while the rendered type was
        // `Option<Vec<String>>` — and the server, told it was reading a scalar, handed the typed
        // read a string it rejected. One source answering both questions is the fix, not a second
        // case in the old match.
        let shape = param_shape(&ty, self.contracts);
        (ty, shape)
    }

    /// The one body the operation sends.
    fn body(&self, operation: &Operation, at: &JsonPointer, ctx: &mut Ctx) -> Option<BodyContract> {
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
                    array: matches!(param_shape(&field.ty, self.contracts), ParamShape::Array),
                });
            }
        }
        // `encoding` entries that name no declared member — a captured `additionalProperties`
        // member can still carry a row — keep the reach the encoding-only version had.
        for (name, (style, explode)) in declared {
            specs.push(FormSpec {
                wire_name: name,
                style,
                explode,
                array: false,
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
    fn select<'a>(
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

    /// The status arms, in the order overlap resolution puts them.
    fn responses(
        &self,
        operation: &Operation,
        at: &JsonPointer,
        ctx: &mut Ctx,
    ) -> ResponseContract {
        let Some(responses) = &operation.responses else {
            return ResponseContract::default();
        };
        let at = at.child("responses");
        let mut used = Namer::default();
        let mut arms = Vec::new();
        for (status, node) in &responses.statuses {
            let Some(pattern) = parse_status(status) else {
                ctx.report(Diagnostic::new(
                    BreakageClass::MalformedMember,
                    Action::Degrade,
                    at.child(status.clone()),
                    format!(
                        "`{status}` is neither a status code nor a range, so no arm is generated \
                         for it"
                    ),
                ));
                continue;
            };
            arms.push(self.arm(pattern, node, &at.child(status.clone()), &mut used, ctx));
        }
        arms.sort_by_key(|arm| arm.status.precedence());

        let default = responses.default.as_ref().map(|node| {
            // The default arm claims whatever no other arm does, so it has no pattern of its own;
            // `Range(0)` is not a status any response can have, which is what makes it usable as
            // the sentinel here without colliding with a real arm.
            self.arm(
                StatusPattern::Range(0),
                node,
                &at.child("default"),
                &mut used,
                ctx,
            )
        });
        ResponseContract { arms, default }
    }

    fn arm(
        &self,
        status: StatusPattern,
        node: &MaybeRef<Response>,
        at: &JsonPointer,
        used: &mut Namer,
        ctx: &mut Ctx,
    ) -> ResponseArm {
        let name = RustIdent::variant(&status_name(status));
        let Some(response) = self.resolved.response(node) else {
            // The decision `miro` forces, made rather than discovered: a dangling *document-level*
            // reference degrades the position it is at, never the operation around it. 612 of these
            // in one document, and skipping an operation per reference would delete the API.
            ctx.report(Diagnostic::new(
                BreakageClass::DanglingRef,
                Action::Degrade,
                at.clone(),
                "the response references a component the document does not declare; the arm is \
                 generated with its body typed as arbitrary JSON rather than dropped",
            ));
            return ResponseArm {
                status,
                ty: TypeRef::Value,
                docs: Docs::default(),
                rust_name: used.unique(name),
            };
        };

        let ty = match response.content.as_ref() {
            // A response with no content is a status and nothing else: 204, and every arm that
            // says only what went wrong.
            None => TypeRef::Unit,
            Some(content) => match Self::select(content, at, ctx) {
                None => TypeRef::Unit,
                Some((media_type, entry)) if is_json(media_type) => entry
                    .schema
                    .map_or(TypeRef::Value, |id| self.type_at(id))
                    .clone(),
                // A body progeny does not type yet arrives as bytes, which is what it is.
                Some(_) => TypeRef::Vec(Box::new(TypeRef::U64)),
            },
        };
        ResponseArm {
            status,
            ty,
            docs: Docs {
                description: response.description.clone(),
                ..Docs::default()
            },
            rust_name: used.unique(name),
        }
    }

    /// The type a schema at an API position became.
    fn type_at(&self, id: SchemaId) -> TypeRef {
        let key = crate::shape::key_of(self.resolved, id);
        self.contracts
            .type_of(&key)
            .cloned()
            .unwrap_or(TypeRef::Value)
    }
}

/// The methods of one path item, in a fixed order.
fn methods(item: &PathItem) -> [(Method, Option<&Operation>); 8] {
    [
        (Method::Get, item.get.as_ref()),
        (Method::Put, item.put.as_ref()),
        (Method::Post, item.post.as_ref()),
        (Method::Delete, item.delete.as_ref()),
        (Method::Options, item.options.as_ref()),
        (Method::Head, item.head.as_ref()),
        (Method::Patch, item.patch.as_ref()),
        (Method::Trace, item.trace.as_ref()),
    ]
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
fn is_json(media_type: &str) -> bool {
    let base = media_type.split(';').next().unwrap_or(media_type).trim();
    base == "application/json" || base.ends_with("+json")
}

/// A status key as a pattern, or nothing when it is neither.
fn parse_status(key: &str) -> Option<StatusPattern> {
    if let Ok(code) = key.parse::<u16>() {
        // Anything outside the range HTTP defines is a typo rather than a status.
        return (100..600)
            .contains(&code)
            .then_some(StatusPattern::Exact(code));
    }
    let mut characters = key.chars();
    let first = characters.next()?.to_digit(10)?;
    let rest: String = characters.collect();
    if !rest.eq_ignore_ascii_case("xx") || !(1..=5).contains(&first) {
        return None;
    }
    u8::try_from(first).ok().map(StatusPattern::Range)
}

/// The variant name an arm contributes.
fn status_name(status: StatusPattern) -> String {
    match status {
        StatusPattern::Exact(code) => match code {
            200 => "Ok".to_owned(),
            201 => "Created".to_owned(),
            202 => "Accepted".to_owned(),
            204 => "NoContent".to_owned(),
            400 => "BadRequest".to_owned(),
            401 => "Unauthorized".to_owned(),
            403 => "Forbidden".to_owned(),
            404 => "NotFound".to_owned(),
            409 => "Conflict".to_owned(),
            422 => "UnprocessableEntity".to_owned(),
            429 => "TooManyRequests".to_owned(),
            500 => "InternalServerError".to_owned(),
            other => format!("Status{other}"),
        },
        StatusPattern::Range(0) => "Default".to_owned(),
        StatusPattern::Range(hundreds) => format!("Status{hundreds}xx"),
    }
}

fn docs_of(operation: &Operation) -> Docs {
    Docs {
        title: operation.summary.clone(),
        description: operation.description.clone(),
        deprecated: operation.deprecated.unwrap_or(false),
    }
}

/// Which of the three serialization shapes a parameter's *type* has.
///
/// The type, not the classified schema shape, because the two can disagree and the type is the one
/// the extraction decodes into: orb's `status[]` is a nullable array, which classifies as a union
/// rather than as `Shape::Array`, so the old shape-side derivation said "primitive" while the
/// rendered field was `Option<Vec<String>>` — and the server rejected the very requests its own
/// client builds. Wrappers that do not change what the wire carries are looked through; named
/// types are looked *into*, because an alias of an array is an array on the wire.
fn param_shape(ty: &TypeRef, contracts: &Contracts) -> ParamShape {
    match ty {
        TypeRef::Option(inner) | TypeRef::Boxed(inner) => param_shape(inner, contracts),
        TypeRef::Vec(_) | TypeRef::Array(..) | TypeRef::Tuple(_) => ParamShape::Array,
        TypeRef::Map(_) => ParamShape::Object,
        TypeRef::Named(index) => match contracts
            .get(*index)
            .map(crate::contract::TypeContract::kind)
        {
            Some(ContractKind::Struct { .. }) => ParamShape::Object,
            Some(ContractKind::Tuple { .. }) => ParamShape::Array,
            Some(ContractKind::Newtype { inner } | ContractKind::Alias { target: inner }) => {
                param_shape(inner, contracts)
            }
            // String enums and unions serialize as whatever scalar their value is; `None` is an
            // unresolvable index, which the type path renderer also treats as opaque.
            Some(ContractKind::StringEnum { .. } | ContractKind::Enum { .. }) | None => {
                ParamShape::Primitive
            }
        },
        TypeRef::Unit
        | TypeRef::Bool
        | TypeRef::I64
        | TypeRef::U64
        | TypeRef::F64
        | TypeRef::String
        | TypeRef::Format(_)
        | TypeRef::Value => ParamShape::Primitive,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_status, preference};
    use crate::api::tests::{model_of, with_paths};
    use crate::api::{BodyContract, Location, StatusPattern};
    use crate::contract::TypeRef;

    #[test]
    fn a_status_key_is_a_code_or_a_range_and_nothing_else() {
        assert_eq!(parse_status("200"), Some(StatusPattern::Exact(200)));
        assert_eq!(parse_status("2XX"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("2xx"), Some(StatusPattern::Range(2)));
        assert_eq!(parse_status("999"), None);
        assert_eq!(parse_status("0XX"), None);
        assert_eq!(parse_status("x-vendor"), None);
        assert_eq!(parse_status(""), None);
    }

    #[test]
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

    #[test]
    fn an_operation_with_no_operation_id_is_named_after_its_method_and_path() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets/{petId}": {
                "get": {
                    "parameters": [{"name": "petId", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })));
        assert_eq!(model.operations()[0].rust_name.as_str(), "get_pets_pet_id");
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::CollidingOperationId)
        );
    }

    #[test]
    fn two_operations_that_want_one_name_are_told_apart_and_reported() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/a": {"get": {"operationId": "list-pets", "responses": {"200": {"description": "ok"}}}},
            "/b": {"get": {"operationId": "list_pets", "responses": {"200": {"description": "ok"}}}},
        })));
        let names: Vec<&str> = model
            .operations()
            .iter()
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(names, ["list_pets", "list_pets2"]);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|found| found.class() == crate::BreakageClass::CollidingOperationId)
                .count(),
            1
        );
    }

    #[test]
    fn a_path_variable_nothing_declares_takes_its_operation_with_it() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets/{petId}": {"get": {"operationId": "getPet", "responses": {"200": {"description": "ok"}}}},
        })));
        assert!(model.operations().is_empty());
        let found = diagnostics
            .iter()
            .find(|found| found.class() == crate::BreakageClass::UnregistrableRoute)
            .expect("the unfillable template should be reported");
        assert!(found.detail().contains("petId"), "{found}");
    }

    #[test]
    fn an_optional_parameter_with_no_defined_serialization_is_dropped_and_the_operation_stays() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "parameters": [
                        {"name": "filter", "in": "query", "style": "deepObject",
                         "schema": {"type": "array", "items": {"type": "string"}}},
                        {"name": "limit", "in": "query", "schema": {"type": "integer"}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })));
        assert_eq!(model.operations().len(), 1);
        let names: Vec<&str> = model.operations()[0]
            .params
            .iter()
            .map(|param| param.wire_name.as_str())
            .collect();
        assert_eq!(names, ["limit"]);
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::QuerySerializationStyle)
        );
    }

    #[test]
    fn a_required_parameter_with_no_defined_serialization_takes_the_operation_with_it() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "parameters": [
                        {"name": "filter", "in": "query", "required": true, "style": "deepObject",
                         "schema": {"type": "array", "items": {"type": "string"}}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })));
        assert!(model.operations().is_empty());
    }

    #[test]
    fn a_parameter_that_wants_a_name_the_builder_uses_takes_a_suffix() {
        // `jellyfin` declares a query parameter called `client`. Every builder has a `client`
        // field, so without a rename the generated struct declares it twice and does not compile —
        // which the tier compile gate is exactly what found.
        let (model, _) = model_of(with_paths(json!({
            "/sessions": {
                "get": {
                    "operationId": "getSessions",
                    "parameters": [
                        {"name": "client", "in": "query", "schema": {"type": "string"}},
                        {"name": "body", "in": "query", "schema": {"type": "string"}},
                    ],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })));
        let params = &model.operations()[0].params;
        let names: Vec<(&str, &str)> = params
            .iter()
            .map(|param| (param.wire_name.as_str(), param.rust_name.as_str()))
            .collect();
        // The wire names are untouched; only the Rust side moves, which is the rule everywhere.
        assert_eq!(names, [("body", "body2"), ("client", "client2")]);
    }

    #[test]
    fn an_operation_level_parameter_overrides_the_path_level_one_it_repeats() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "parameters": [{"name": "limit", "in": "query", "schema": {"type": "string"}}],
                "get": {
                    "operationId": "listPets",
                    "parameters": [{"name": "limit", "in": "query", "required": true, "schema": {"type": "integer"}}],
                    "responses": {"200": {"description": "ok"}},
                },
            },
        })));
        let params = &model.operations()[0].params;
        assert_eq!(params.len(), 1);
        assert!(params[0].required);
        assert_eq!(params[0].ty, TypeRef::I64);
        assert_eq!(params[0].style.location(), Location::Query);
    }

    #[test]
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
        })));
        let upload = model
            .operations()
            .iter()
            .find(|operation| operation.rust_name.as_str() == "upload")
            .unwrap();
        assert!(matches!(
            upload.body,
            Some(BodyContract::Bytes { required: true, .. })
        ));
        let create = model
            .operations()
            .iter()
            .find(|operation| operation.rust_name.as_str() == "create_pet")
            .unwrap();
        assert!(matches!(
            create.body,
            Some(BodyContract::Json {
                ty: TypeRef::Named(_),
                required: false
            })
        ));
    }

    #[test]
    fn a_response_referencing_a_component_that_does_not_exist_degrades_its_arm_not_its_operation() {
        // `miro`, in miniature: a reference to a `components.responses` section the document never
        // declares.
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": {
                        "200": {"$ref": "#/components/responses/200"},
                        "404": {"description": "gone"},
                    },
                },
            },
        })));
        assert_eq!(model.operations().len(), 1);
        let responses = &model.operations()[0].responses;
        assert_eq!(responses.arms.len(), 2);
        assert_eq!(responses.arms[0].status, StatusPattern::Exact(200));
        assert_eq!(responses.arms[0].ty, TypeRef::Value);
        assert!(
            diagnostics
                .iter()
                .any(|found| found.class() == crate::BreakageClass::DanglingRef)
        );
    }

    #[test]
    fn exact_arms_come_before_the_ranges_they_overlap() {
        let (model, _) = model_of(with_paths(json!({
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": {
                        "5XX": {"description": "broken"},
                        "200": {"description": "ok"},
                        "2XX": {"description": "fine"},
                        "404": {"description": "gone"},
                        "default": {"description": "anything else"},
                    },
                },
            },
        })));
        let responses = &model.operations()[0].responses;
        let patterns: Vec<StatusPattern> = responses.arms.iter().map(|arm| arm.status).collect();
        assert_eq!(
            patterns,
            [
                StatusPattern::Exact(200),
                StatusPattern::Exact(404),
                StatusPattern::Range(2),
                StatusPattern::Range(5),
            ]
        );
        assert!(responses.default.is_some());
        assert_eq!(responses.arms[0].rust_name.as_str(), "Ok");
        assert_eq!(responses.arms[2].rust_name.as_str(), "Status2xx");
    }
}
