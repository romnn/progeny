//! The API model: the catalogue of operations, and the only thing the client and server renderers
//! read.
//!
//! The third representation. It refers to types by [`TypeRef`] rather than restating them, so the
//! calling side, the serving side and the shared type layer cannot disagree about what a body is —
//! there is one answer and all three read it.
//!
//! Like the type contracts, this is **finalized before rendering**: a renderer receives
//! `&ApiModel` and has nowhere to put a decision. Every choice that affects the request on the
//! wire — which media type a body uses, how a parameter serializes, which status arms exist and
//! which one an undeclared status falls into — is made here and stored as data.
//!
//! What this layer adds that no earlier one has is **position**. A schema in the type layer is a
//! shape and nothing else; here it is a request body, or a `404` response, or a query parameter,
//! and several facts that were unanswerable before become ordinary: whether an
//! optional-and-nullable collapse is on the way in or on the way out, whether a type is on the
//! wire at all, and whether an example contradicts the schema it illustrates.

mod examples;
mod operation;
mod pagination;
// The payload gate is the only caller, and it is feature-gated. The module stays compiled in every
// test build so its own tests keep running.
#[cfg(any(feature = "harness", test))]
mod payload;
mod presence;
mod registrable;
mod route;
mod style;

#[cfg(feature = "harness")]
pub(crate) use payload::{Skipped, collect as payloads};

use crate::config::Config;
use crate::contract::{Contracts, RustIdent, TypeRef};
use crate::diag::{Ctx, JsonPointer};
use crate::resolve::ResolvedDocument;
use crate::shape::{Docs, Shapes};

pub(crate) use pagination::PaginationContract;
pub(crate) use registrable::RegistrableRoute;
pub(crate) use route::{PathTemplate, Piece};
pub(crate) use style::{Location, Style, StyleContract};

/// Every operation a document declares, and the servers it declares them against.
#[derive(Debug, Default)]
pub(crate) struct ApiModel {
    operations: Vec<OperationContract>,
    servers: Vec<ServerEntry>,
}

impl ApiModel {
    pub(crate) fn operations(&self) -> &[OperationContract] {
        &self.operations
    }

    /// The server URL a generated `Default` can point at, when the document declares one.
    ///
    /// The first declared server, and only when its URL is usable exactly as written. Two shapes
    /// are declined, and the constructor still takes either: a *templated* URL has variables
    /// progeny does not substitute, and a *relative* one — `/api/v3`, which OpenAPI allows and
    /// means "relative to wherever this description is served" — is not a URL at all once the
    /// description is a file on disk, and only the caller knows what it was relative to.
    ///
    /// Decided here rather than in the renderer, because which URLs are usable is a fact about
    /// the document; the renderer's job is only to spell the `impl Default` when there is one.
    pub(crate) fn default_server_url(&self) -> Option<&str> {
        let url = self.servers.first()?.url.as_str();
        (!url.contains('{') && (url.starts_with("https://") || url.starts_with("http://")))
            .then_some(url)
    }
}

/// A base URL the document offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerEntry {
    pub(crate) url: String,
    pub(crate) description: Option<String>,
}

/// One callable operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OperationContract {
    pub(crate) rust_name: RustIdent,
    pub(crate) method: Method,
    pub(crate) path: PathTemplate,
    pub(crate) params: Vec<ParamContract>,
    pub(crate) body: Option<BodyContract>,
    pub(crate) responses: ResponseContract,
    pub(crate) docs: Docs,
    /// The route a router accepted for this operation, when it accepted one.
    ///
    /// `None` is a collision, and it costs the operation its place in the generated *server* and
    /// nothing else: a colliding route is still perfectly callable, and only a router has to tell
    /// two of them apart. Carried as a type only the classifier constructs, so an unregistrable
    /// route cannot reach the server renderer at all.
    pub(crate) registrable: Option<RegistrableRoute>,
    /// Whether this method sends one of the further media types its request body position
    /// declares, rather than the primary.
    ///
    /// A variant is a client method only: it shares its route and method with the primary, and a
    /// router that registered both would be registering one route twice. The generated server
    /// accepts the primary's media type, which the `multi-media-type` diagnostic states.
    pub(crate) body_variant: bool,
    /// How this operation paginates, when the configuration declared it and the document supported
    /// the declaration. Never detected — see [`pagination`].
    pub(crate) pagination: Option<PaginationContract>,
    /// Where the operation was written, for diagnostics that point at it.
    pub(crate) origin: JsonPointer,
}

impl OperationContract {
    /// The parameters at one location.
    ///
    /// Ordered by wire name rather than by where the document wrote them: an operation's parameters
    /// come from two places — the path item and the operation — and merging them by `(name, in)` is
    /// what implements the specification's override rule, which leaves no single declaration order
    /// to preserve. Deterministic is what the output invariant actually needs.
    pub(crate) fn params_at(&self, location: Location) -> impl Iterator<Item = &ParamContract> {
        self.params
            .iter()
            .filter(move |param| param.style.location() == location)
    }
}

// The method enum lives beside `PathItem`, whose fields it mirrors, so the document walk can
// yield it with no fallback arm; the API model is where every consumer reads it from.
pub(crate) use crate::doc::Method;

/// One parameter, with the serialization already decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParamContract {
    pub(crate) rust_name: RustIdent,
    /// The exact bytes the parameter has on the wire.
    pub(crate) wire_name: String,
    /// The validated (location, style, explode, shape) combination.
    pub(crate) style: StyleContract,
    pub(crate) required: bool,
    pub(crate) ty: TypeRef,
    pub(crate) docs: Docs,
}

/// The one body a request carries, and how.
///
/// One media type per operation, chosen by the preference table in [`operation`]: generating one
/// faithful body beats generating several half-faithful ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodyContract {
    Json {
        ty: TypeRef,
        required: bool,
        /// The declared media type, exactly as the document spells it. `application/json` for
        /// most positions — but a `+json` structured-syntax type (`application/vnd.api+json`,
        /// a versioned vendor type) is the position's actual wire contract, and the client
        /// sends the declared spelling rather than the family default.
        content_type: String,
    },
    /// `application/x-www-form-urlencoded`: the same typed value as a JSON body, encoded with the
    /// form style row instead of as JSON. 109 bodies across 11 corpus documents, 103 of them an
    /// object with declared properties.
    Form {
        ty: TypeRef,
        /// What the body's `encoding` said about individual members, when it said anything. Empty
        /// for all but one corpus body.
        specs: Vec<FormSpec>,
        required: bool,
    },
    /// `multipart/form-data`: one part per member of the typed value. 278 bodies across 27 corpus
    /// documents, which makes it the largest non-JSON body by a wide margin.
    Multipart {
        ty: TypeRef,
        /// What the document was specific about, member by member. Not the list of members: an
        /// `additionalProperties` member is real and absent from it, so the runtime walks the value
        /// and consults this rather than the other way round.
        parts: Vec<PartSpec>,
        required: bool,
    },
    /// A body that is text on the wire: `text/plain` and its relatives. 14 bodies across 5
    /// documents, all of them declaring `text/plain` and nothing else.
    Text {
        content_type: String,
        required: bool,
    },
    /// Anything progeny does not yet give a typed form: sent as bytes under the media type the
    /// document declared, which is faithful about the request and silent about its shape.
    Bytes {
        content_type: String,
        required: bool,
    },
}

impl BodyContract {
    pub(crate) fn required(&self) -> bool {
        match self {
            Self::Json { required, .. }
            | Self::Form { required, .. }
            | Self::Multipart { required, .. }
            | Self::Text { required, .. }
            | Self::Bytes { required, .. } => *required,
        }
    }

    /// The type the builder's `body` field holds, when the body has one.
    pub(crate) fn ty(&self) -> Option<&TypeRef> {
        match self {
            Self::Json { ty, .. } | Self::Form { ty, .. } | Self::Multipart { ty, .. } => Some(ty),
            Self::Text { .. } | Self::Bytes { .. } => None,
        }
    }
}

/// How one member of a multipart body becomes a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartKind {
    /// Written as its text.
    Text,
    /// Written as its bytes, with a `filename`: what the document marked binary.
    File,
    /// Written as JSON, which is what every corpus body that names a `contentType` for a structured
    /// member names.
    Json,
}

/// What the document said about one member of a multipart body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartSpec {
    pub(crate) wire_name: String,
    pub(crate) kind: PartKind,
    /// Whether the member is an array, and therefore becomes one part per element.
    ///
    /// Carried beside the kind rather than folded into it: the kind describes what one part holds
    /// and this describes how many there are, and a member typed `Value` that happens to hold an
    /// array at run time is one JSON part rather than several.
    pub(crate) repeated: bool,
    /// The per-part content type the body's `encoding` declared, if it declared one.
    pub(crate) content_type: Option<String>,
}

/// What the document's `encoding` said about one member of a form body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormSpec {
    pub(crate) wire_name: String,
    pub(crate) style: Style,
    pub(crate) explode: bool,
    /// Whether the member's schema says it is an array — the same fact a query parameter carries
    /// in its [`StyleContract`], for the same reason: a single-occurrence exploded array on the
    /// wire is byte-identical to a scalar, and only the schema can tell the reader which it was.
    ///
    /// `None` when the schema does not declare the member at all — an `encoding` row can name a
    /// member captured only through `additionalProperties` — and the reader falls back to the
    /// repetition heuristic rather than trusting a guess.
    pub(crate) array: Option<bool>,
}

/// What each declared status yields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResponseContract {
    /// Exact codes and ranges, in the order overlap resolution puts them: exact before range.
    pub(crate) arms: Vec<ResponseArm>,
    /// The `default` arm, which catches what no other arm claims.
    pub(crate) default: Option<ResponseArm>,
    /// Media types [`Config::media_types`](crate::config::Config::media_types) chose at this
    /// operation's response positions, deduplicated in arm order.
    ///
    /// The response media type is the server's choice, and configuring one is only a statement
    /// about what the client will decode — unless the request says so too. These become the
    /// `Accept` header, so the server the configuration argues with actually hears the argument.
    pub(crate) accept: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResponseArm {
    pub(crate) status: StatusPattern,
    pub(crate) body: ResponseBody,
    pub(crate) docs: Docs,
    /// The variant name this arm contributes to the response enum.
    pub(crate) rust_name: RustIdent,
}

/// What one response arm carries on the wire.
///
/// The kind owns the payload type where the wire has one, so a JSON decoder cannot accidentally be
/// paired with a raw body. Text and bytes have no schema-derived Rust type: their wire kind decides
/// the representation directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResponseBody {
    Json {
        ty: TypeRef,
        /// The exact declared spelling — `application/problem+json`, a vendor type — which is
        /// what the generated server writes; `application/json` is only the common case.
        content_type: String,
    },
    Text {
        content_type: String,
    },
    Bytes {
        content_type: String,
    },
    Empty,
}

impl ResponseBody {
    /// The schema-derived type reached by this response, when the response is JSON.
    pub(crate) fn json_type(&self) -> Option<&TypeRef> {
        match self {
            Self::Json { ty, .. } => Some(ty),
            Self::Text { .. } | Self::Bytes { .. } | Self::Empty => None,
        }
    }
}

/// Which statuses an arm claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum StatusPattern {
    Exact(u16),
    /// An OpenAPI range key: `2XX` is `Range(2)`.
    Range(u8),
}

impl StatusPattern {
    /// Whether this pattern claims a successful status.
    pub(crate) fn is_success(self) -> bool {
        match self {
            Self::Exact(code) => (200..300).contains(&code),
            Self::Range(hundreds) => hundreds == 2,
        }
    }

    /// Exact before range, so overlap resolution is the order rather than a rule a reader has to
    /// remember. Within each, ascending.
    fn precedence(self) -> (u8, u16) {
        match self {
            Self::Exact(code) => (0, code),
            Self::Range(hundreds) => (1, u16::from(hundreds)),
        }
    }
}

/// Build the API model.
///
/// Runs whether or not a client or a server will be rendered: the diagnostics it produces are
/// findings about the document, and a caller that asked for types only is still entitled to know
/// that two operations collide or that an example contradicts its schema.
pub(crate) fn build(
    resolved: &ResolvedDocument,
    shapes: &Shapes,
    contracts: &Contracts,
    config: &Config,
    ctx: &mut Ctx,
) -> Result<ApiModel, crate::diag::RejectError> {
    let mut model = operation::run(resolved, contracts, config, ctx)?;
    model.servers = servers(resolved);
    // After the operations exist and before anything renders: a declaration that the document
    // cannot support has to stop generation rather than produce a method that cannot work.
    pagination::attach(&mut model.operations, contracts, config)?;
    presence::report(contracts, &model, ctx);
    examples::report(resolved, shapes, ctx);
    Ok(model)
}

fn servers(resolved: &ResolvedDocument) -> Vec<ServerEntry> {
    resolved
        .document()
        .servers
        .iter()
        .flatten()
        .filter_map(|server| {
            Some(ServerEntry {
                url: server.url.clone()?,
                description: server.description.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use serde_json::{Value, json};

    use super::{ApiModel, build};
    use crate::config::Config;
    use crate::diag::{Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::{contract, normalize, resolve, shape};

    /// Build the API model for a document, keeping what was said about it.
    pub(crate) fn model_of(document: Value) -> eyre::Result<(ApiModel, Vec<Diagnostic>)> {
        model_with(document, &Config::default())
    }

    /// The same, under a caller-supplied configuration.
    pub(crate) fn model_with(
        document: Value,
        config: &Config,
    ) -> eyre::Result<(ApiModel, Vec<Diagnostic>)> {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx)?;
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, config, &mut ctx)?;
        let model = build(&resolved, &shapes, &contracts, config, &mut ctx)?;
        Ok((model, ctx.into_diagnostics()))
    }

    /// A document with the given paths object.
    pub(crate) fn with_paths(paths: Value) -> Value {
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), paths);
        Value::Object(root)
    }

    #[test_util::test]
    fn an_operation_is_named_after_its_operation_id() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {"get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}}},
        })))?;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(model.operations().len(), 1);
        assert_eq!(model.operations()[0].rust_name.as_str(), "list_pets");
        assert_eq!(model.operations()[0].method, super::Method::Get);
    }

    #[test_util::test]
    fn servers_are_carried_for_the_client_to_default_to() {
        let mut document = with_paths(json!({
            "/pets": {"get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}}},
        }));
        document
            .as_object_mut()
            .ok_or_eyre("test fixture should contain this value")?
            .insert(
                "servers".to_owned(),
                json!([{"url": "https://example.invalid/v1", "description": "production"}]),
            );
        let (model, _) = model_of(document)?;
        assert_eq!(model.servers.len(), 1);
        assert_eq!(model.servers[0].url, "https://example.invalid/v1");
        assert_eq!(
            model.default_server_url(),
            Some("https://example.invalid/v1")
        );
    }
}
