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
// combination so its own tests keep running; only the re-export follows the feature.
#[cfg_attr(
    not(feature = "harness"),
    allow(
        dead_code,
        reason = "the payload gate is the only caller so far, and it is feature-gated"
    )
)]
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

    pub(crate) fn servers(&self) -> &[ServerEntry] {
        &self.servers
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

/// An HTTP method, as a closed set: a renderer cannot be handed one that is not a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Method {
    Get,
    Put,
    Post,
    Delete,
    Options,
    Head,
    Patch,
    Trace,
}

impl Method {
    /// The uppercase name the request line carries.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Post => "POST",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Head => "HEAD",
            Self::Patch => "PATCH",
            Self::Trace => "TRACE",
        }
    }

    /// The lowercase name the document writes, and the one an operation is named after.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::Post => "post",
            Self::Delete => "delete",
            Self::Options => "options",
            Self::Head => "head",
            Self::Patch => "patch",
            Self::Trace => "trace",
        }
    }
}

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
    pub(crate) array: bool,
}

/// What each declared status yields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResponseContract {
    /// Exact codes and ranges, in the order overlap resolution puts them: exact before range.
    pub(crate) arms: Vec<ResponseArm>,
    /// The `default` arm, which catches what no other arm claims.
    pub(crate) default: Option<ResponseArm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResponseArm {
    pub(crate) status: StatusPattern,
    pub(crate) ty: TypeRef,
    pub(crate) docs: Docs,
    /// The variant name this arm contributes to the response enum.
    pub(crate) rust_name: RustIdent,
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
    let mut model = operation::run(resolved, contracts, config, ctx);
    model.servers = servers(resolved);
    // After the operations exist and before anything renders: a declaration that the document
    // cannot support has to stop generation rather than produce a method that cannot work.
    pagination::attach(&mut model.operations, contracts, config)?;
    presence::report(contracts, &model, ctx);
    examples::report(resolved, shapes, contracts, ctx);
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
    use serde_json::{Value, json};

    use super::{ApiModel, build};
    use crate::config::Config;
    use crate::diag::{Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::{contract, normalize, resolve, shape};

    /// Build the API model for a document, keeping what was said about it.
    pub(crate) fn model_of(document: Value) -> (ApiModel, Vec<Diagnostic>) {
        let config = Config::default();
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve::resolve(parsed, &mut ctx);
        let shapes = shape::classify(&resolved, &mut ctx);
        let contracts = contract::build(&resolved, &shapes, &config, &mut ctx).unwrap();
        let model = build(&resolved, &shapes, &contracts, &config, &mut ctx).unwrap();
        (model, ctx.into_diagnostics())
    }

    /// A document with the given paths object.
    pub(crate) fn with_paths(paths: Value) -> Value {
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), paths);
        Value::Object(root)
    }

    #[test]
    fn an_operation_is_named_after_its_operation_id() {
        let (model, diagnostics) = model_of(with_paths(json!({
            "/pets": {"get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}}},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(model.operations().len(), 1);
        assert_eq!(model.operations()[0].rust_name.as_str(), "list_pets");
        assert_eq!(model.operations()[0].method, super::Method::Get);
    }

    #[test]
    fn servers_are_carried_for_the_client_to_default_to() {
        let mut document = with_paths(json!({
            "/pets": {"get": {"operationId": "listPets", "responses": {"200": {"description": "ok"}}}},
        }));
        document.as_object_mut().unwrap().insert(
            "servers".to_owned(),
            json!([{"url": "https://example.invalid/v1", "description": "production"}]),
        );
        let (model, _) = model_of(document);
        assert_eq!(model.servers().len(), 1);
        assert_eq!(model.servers()[0].url, "https://example.invalid/v1");
    }
}
