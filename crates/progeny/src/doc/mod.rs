//! The lossless document model: a typed OpenAPI 3.1 tree.
//!
//! Two rules shape every node here.
//!
//! **Every node carries its uninterpreted members.** `x-*` extensions, members from a version
//! that does not exist yet, and members whose value had the wrong shape all live in
//! `extensions`, verbatim. That is what makes the model a superset rather than a filter, and it
//! is why a document can round-trip through it unchanged.
//!
//! **Optional means optional, including where OpenAPI says required.** `Response::description`
//! is required by the specification and missing from real documents; typing it as `Option`
//! means a document that omits it is read, diagnosed by the layer that needs it, and written
//! back as it was — rather than rejected at the edge, which would be the harshest possible
//! response to the most common defect.

pub(crate) mod parse;
// Serializing the model back to a value is what makes the round-trip property checkable, and
// today the corpus harness is its only caller. `allow` rather than `expect` because the lint fires
// only in the feature combinations that leave the harness out.
#[cfg_attr(
    not(feature = "harness"),
    allow(
        dead_code,
        reason = "the round-trip check is the only caller so far, and it is feature-gated"
    )
)]
pub(crate) mod serialize;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::schema::{ExternalDocs, SchemaId, SchemaStore};

/// A parsed document, together with the arena holding every schema it mentions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedDocument {
    pub(crate) document: Document,
    pub(crate) schemas: SchemaStore,
}

/// Either a reference to a component or the component itself.
///
/// A reference is a distinct shape rather than an optional field so that a later layer cannot
/// read a response's `description` out of a node that is really a pointer somewhere else.
/// Schemas are the deliberate exception: 2020-12 gives `$ref` siblings meaning, so a schema's
/// reference is one of its keywords.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MaybeRef<T> {
    Reference(Reference),
    Item(T),
}

/// A Reference Object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reference {
    /// The `$ref` string, held as written.
    pub(crate) target: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Document {
    pub(crate) openapi: Option<String>,
    pub(crate) info: Option<Info>,
    pub(crate) json_schema_dialect: Option<String>,
    pub(crate) servers: Option<Vec<Server>>,
    pub(crate) paths: Option<Paths>,
    pub(crate) webhooks: Option<BTreeMap<String, MaybeRef<PathItem>>>,
    pub(crate) components: Option<Components>,
    pub(crate) security: Option<Vec<SecurityRequirement>>,
    pub(crate) tags: Option<Vec<Tag>>,
    pub(crate) external_docs: Option<ExternalDocs>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Info {
    pub(crate) title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) terms_of_service: Option<String>,
    pub(crate) contact: Option<Contact>,
    pub(crate) license: Option<License>,
    pub(crate) version: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Contact {
    pub(crate) name: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct License {
    pub(crate) name: Option<String>,
    pub(crate) identifier: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Server {
    pub(crate) url: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) variables: Option<BTreeMap<String, ServerVariable>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServerVariable {
    pub(crate) enumeration: Option<Vec<String>>,
    pub(crate) default: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// The Paths Object: an open map whose keys are path templates.
///
/// Modelled as its own node rather than a bare map because the object itself may carry `x-*`
/// members, and because a member that is not a path template has to go somewhere that survives
/// serialization.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Paths {
    pub(crate) items: BTreeMap<String, MaybeRef<PathItem>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PathItem {
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) get: Option<Operation>,
    pub(crate) put: Option<Operation>,
    pub(crate) post: Option<Operation>,
    pub(crate) delete: Option<Operation>,
    pub(crate) options: Option<Operation>,
    pub(crate) head: Option<Operation>,
    pub(crate) patch: Option<Operation>,
    pub(crate) trace: Option<Operation>,
    pub(crate) servers: Option<Vec<Server>>,
    pub(crate) parameters: Option<Vec<MaybeRef<Parameter>>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

impl PathItem {
    /// Every operation this path item declares, with the method member it was written under.
    ///
    /// The one walk over the method fields. Seven copies of this list used to exist across the
    /// crate, and the failure mode of a copy missing a method is the forbidden one: the
    /// operation's examples, payloads and statistics silently vanish from their gates. A method
    /// added to this struct without extending this list is caught by the mirror test beside the
    /// parser, which counts the members a maximal path item round-trips.
    pub(crate) fn operations(&self) -> impl Iterator<Item = (Method, &Operation)> {
        [
            (Method::Get, &self.get),
            (Method::Put, &self.put),
            (Method::Post, &self.post),
            (Method::Delete, &self.delete),
            (Method::Options, &self.options),
            (Method::Head, &self.head),
            (Method::Patch, &self.patch),
            (Method::Trace, &self.trace),
        ]
        .into_iter()
        .filter_map(|(method, operation)| Some((method, operation.as_ref()?)))
    }
}

/// An HTTP method, as a closed set: a renderer cannot be handed one that is not a method.
///
/// Defined beside [`PathItem`], whose fields are the eight members a document can write, so that
/// [`PathItem::operations`] can yield it directly — a string-to-method mapping anywhere else
/// would need a fallback arm, and every wrong answer for that arm is one of the forbidden ones:
/// a panic, or an operation silently skipped.
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

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Operation {
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) external_docs: Option<ExternalDocs>,
    pub(crate) id: Option<String>,
    pub(crate) parameters: Option<Vec<MaybeRef<Parameter>>>,
    pub(crate) request_body: Option<MaybeRef<RequestBody>>,
    pub(crate) responses: Option<Responses>,
    pub(crate) callbacks: Option<BTreeMap<String, MaybeRef<Callback>>>,
    pub(crate) deprecated: Option<bool>,
    pub(crate) security: Option<Vec<SecurityRequirement>>,
    pub(crate) servers: Option<Vec<Server>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Parameter {
    pub(crate) name: Option<String>,
    /// The `in` member, held as written. Which locations mean something is the API model's
    /// business; an unknown one has to survive to be diagnosed there.
    pub(crate) location: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) required: Option<bool>,
    pub(crate) deprecated: Option<bool>,
    pub(crate) allow_empty_value: Option<bool>,
    pub(crate) style: Option<String>,
    pub(crate) explode: Option<bool>,
    pub(crate) allow_reserved: Option<bool>,
    pub(crate) schema: Option<SchemaId>,
    pub(crate) example: Option<Value>,
    pub(crate) examples: Option<BTreeMap<String, MaybeRef<Example>>>,
    pub(crate) content: Option<BTreeMap<String, MediaType>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// A Header Object: a parameter without a `name` or an `in`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Header {
    pub(crate) description: Option<String>,
    pub(crate) required: Option<bool>,
    pub(crate) deprecated: Option<bool>,
    pub(crate) allow_empty_value: Option<bool>,
    pub(crate) style: Option<String>,
    pub(crate) explode: Option<bool>,
    pub(crate) allow_reserved: Option<bool>,
    pub(crate) schema: Option<SchemaId>,
    pub(crate) example: Option<Value>,
    pub(crate) examples: Option<BTreeMap<String, MaybeRef<Example>>>,
    pub(crate) content: Option<BTreeMap<String, MediaType>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RequestBody {
    pub(crate) description: Option<String>,
    pub(crate) content: Option<BTreeMap<String, MediaType>>,
    pub(crate) required: Option<bool>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MediaType {
    pub(crate) schema: Option<SchemaId>,
    pub(crate) example: Option<Value>,
    pub(crate) examples: Option<BTreeMap<String, MaybeRef<Example>>>,
    pub(crate) encoding: Option<BTreeMap<String, Encoding>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Encoding {
    pub(crate) content_type: Option<String>,
    pub(crate) headers: Option<BTreeMap<String, MaybeRef<Header>>>,
    pub(crate) style: Option<String>,
    pub(crate) explode: Option<bool>,
    pub(crate) allow_reserved: Option<bool>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// The Responses Object: `default` plus an open map keyed by status code or range.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Responses {
    pub(crate) default: Option<MaybeRef<Response>>,
    pub(crate) statuses: BTreeMap<String, MaybeRef<Response>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Response {
    pub(crate) description: Option<String>,
    pub(crate) headers: Option<BTreeMap<String, MaybeRef<Header>>>,
    pub(crate) content: Option<BTreeMap<String, MediaType>>,
    pub(crate) links: Option<BTreeMap<String, MaybeRef<Link>>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// A Callback Object: an open map keyed by runtime expression.
///
/// Carried losslessly and not interpreted by later layers, which report it rather than pretend
/// to support it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Callback {
    pub(crate) expressions: BTreeMap<String, MaybeRef<PathItem>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Example {
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) value: Option<Value>,
    pub(crate) external_value: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// A Link Object. Carried losslessly and not interpreted by later layers.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Link {
    pub(crate) operation_ref: Option<String>,
    pub(crate) operation_id: Option<String>,
    pub(crate) parameters: Option<BTreeMap<String, Value>>,
    pub(crate) request_body: Option<Value>,
    pub(crate) description: Option<String>,
    pub(crate) server: Option<Server>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Components {
    pub(crate) schemas: Option<BTreeMap<String, SchemaId>>,
    pub(crate) responses: Option<BTreeMap<String, MaybeRef<Response>>>,
    pub(crate) parameters: Option<BTreeMap<String, MaybeRef<Parameter>>>,
    pub(crate) examples: Option<BTreeMap<String, MaybeRef<Example>>>,
    pub(crate) request_bodies: Option<BTreeMap<String, MaybeRef<RequestBody>>>,
    pub(crate) headers: Option<BTreeMap<String, MaybeRef<Header>>>,
    pub(crate) security_schemes: Option<BTreeMap<String, MaybeRef<SecurityScheme>>>,
    pub(crate) links: Option<BTreeMap<String, MaybeRef<Link>>>,
    pub(crate) callbacks: Option<BTreeMap<String, MaybeRef<Callback>>>,
    pub(crate) path_items: Option<BTreeMap<String, MaybeRef<PathItem>>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tag {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) external_docs: Option<ExternalDocs>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

/// A Security Requirement Object: scheme name to the scopes it needs.
///
/// An empty map is meaningful — it makes security optional for the operation — so this is never
/// collapsed with an absent `security` member.
pub(crate) type SecurityRequirement = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SecurityScheme {
    /// The `type` member, held as written.
    pub(crate) kind: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) name: Option<String>,
    /// The `in` member, held as written.
    pub(crate) location: Option<String>,
    pub(crate) scheme: Option<String>,
    pub(crate) bearer_format: Option<String>,
    pub(crate) flows: Option<OAuthFlows>,
    pub(crate) open_id_connect_url: Option<String>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OAuthFlows {
    pub(crate) implicit: Option<OAuthFlow>,
    pub(crate) password: Option<OAuthFlow>,
    pub(crate) client_credentials: Option<OAuthFlow>,
    pub(crate) authorization_code: Option<OAuthFlow>,
    pub(crate) extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OAuthFlow {
    pub(crate) authorization_url: Option<String>,
    pub(crate) token_url: Option<String>,
    pub(crate) refresh_url: Option<String>,
    pub(crate) scopes: Option<BTreeMap<String, String>>,
    pub(crate) extensions: BTreeMap<String, Value>,
}
