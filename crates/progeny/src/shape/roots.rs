//! Which schemas need a name, and what to call them.
//!
//! A name has to be derivable from the document rather than from the order the walk happened to
//! reach a schema in, or checked-in generated output produces unreadable diffs whenever an
//! unrelated path is added. So every named position is discovered here, from the document, with the
//! name it suggests: a component gives its own name, and an inline schema takes the operation's
//! name plus where in the operation it sits.
//!
//! Two positions are deliberately not roots. **Callbacks** are carried losslessly and not
//! interpreted by any later layer ([`crate::doc::Callback`]), so naming their payloads would emit
//! types nothing refers to. **Links** hold no schemas at all. Webhooks *are* roots: a webhook
//! payload is a payload, even though the operation itself is not rendered in v1.

use crate::doc::{Document, MaybeRef, MediaType, Operation, PathItem};
use crate::resolve::ResolvedDocument;
use crate::schema::SchemaId;

use super::ShapeKey;

/// Where a named shape came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootKind {
    /// A `components.schemas` entry: the document named it, so progeny uses that name.
    Component,
    /// A schema at a position an operation really sends or receives.
    ///
    /// The distinction from [`RootKind::Named`] is load-bearing rather than descriptive: a
    /// discriminated union may only consume its tag when no variant type is used anywhere the tag
    /// property is genuinely on the wire, and this is what "on the wire" means
    /// ([`super::discriminate`]).
    Position,
    /// A schema named for the sake of having a name, not because a payload carries it.
    ///
    /// A `components.parameters` or `components.responses` entry the document declares and no
    /// operation references is exactly as much on the wire as a `components.schemas` entry — which
    /// is to say not at all. Anything an operation *does* reference is reached through the
    /// operation as well, and gets a [`RootKind::Position`] root there.
    Named,
}

/// A schema position that needs a name, before classification.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    pub(crate) id: SchemaId,
    pub(crate) hint: Vec<String>,
    pub(crate) kind: RootKind,
}

/// A named shape.
#[derive(Debug, Clone)]
pub(crate) struct Root {
    pub(crate) key: ShapeKey,
    /// The name's segments, before sanitization: `["createWidget", "response", "200"]`.
    pub(crate) hint: Vec<String>,
    pub(crate) kind: RootKind,
}

/// Every schema position that needs a name, in document order.
pub(crate) fn discover(resolved: &ResolvedDocument) -> Vec<Site> {
    let mut walk = Walk {
        resolved,
        sites: Vec::new(),
        kind: RootKind::Position,
    };
    walk.document(resolved.document());
    walk.sites
}

struct Walk<'a> {
    resolved: &'a ResolvedDocument,
    sites: Vec<Site>,
    /// Which kind the positions reached from here are, so the walk says it once rather than
    /// threading it through every method that can reach a schema.
    kind: RootKind,
}

impl Walk<'_> {
    fn push(&mut self, id: SchemaId, hint: Vec<String>, kind: RootKind) {
        self.sites.push(Site { id, hint, kind });
    }

    fn document(&mut self, document: &Document) {
        // Components first, so that a schema the document named keeps that name even when an
        // inline position reaches the same shape.
        if let Some(components) = &document.components {
            for (name, &id) in components.schemas.iter().flatten() {
                self.push(id, vec![name.clone()], RootKind::Component);
            }
            // The component sections are walked for the *names* they give, with `Named` rather
            // than `Position`: an entry no operation references is no more on the wire than a
            // `components.schemas` entry is, and one that is referenced is reached again through
            // the operation that references it.
            self.kind = RootKind::Named;
            for (name, body) in components.request_bodies.iter().flatten() {
                if let Some(body) = self.resolved.request_body(body) {
                    self.content(body.content.as_ref(), std::slice::from_ref(name));
                }
            }
            for (name, response) in components.responses.iter().flatten() {
                if let Some(response) = self.resolved.response(response) {
                    self.response(response, std::slice::from_ref(name));
                }
            }
            for (name, parameter) in components.parameters.iter().flatten() {
                if let Some(parameter) = self.resolved.parameter(parameter) {
                    if let Some(id) = parameter.schema {
                        self.push(id, vec![name.clone()], RootKind::Named);
                    }
                    self.content(parameter.content.as_ref(), std::slice::from_ref(name));
                }
            }
            for (name, header) in components.headers.iter().flatten() {
                if let Some(header) = self.resolved.header(header) {
                    if let Some(id) = header.schema {
                        self.push(id, vec![name.clone()], RootKind::Named);
                    }
                    self.content(header.content.as_ref(), std::slice::from_ref(name));
                }
            }
            self.kind = RootKind::Position;
        }

        let paths = document
            .paths
            .as_ref()
            .map(|paths| &paths.items)
            .into_iter()
            .flatten();
        let webhooks = document.webhooks.as_ref().into_iter().flatten();
        for (route, item) in paths.chain(webhooks) {
            if let Some(item) = self.resolved.path_item(item) {
                self.path_item(item, route);
            }
        }
    }

    fn path_item(&mut self, item: &PathItem, route: &str) {
        for (method, operation) in [
            ("get", &item.get),
            ("put", &item.put),
            ("post", &item.post),
            ("delete", &item.delete),
            ("options", &item.options),
            ("head", &item.head),
            ("patch", &item.patch),
            ("trace", &item.trace),
        ] {
            if let Some(operation) = operation {
                let name = operation_name(operation, method, route);
                self.operation(operation, &name);
            }
        }
        // Path-level parameters belong to every operation on the route, so they are named from the
        // route rather than from any one operation.
        self.parameters(item.parameters.as_deref(), &route_segments(route));
    }

    fn operation(&mut self, operation: &Operation, name: &[String]) {
        self.parameters(operation.parameters.as_deref(), name);
        if let Some(body) = &operation.request_body
            && let Some(body) = self.resolved.request_body(body)
        {
            self.content(body.content.as_ref(), &extend(name, "body"));
        }
        if let Some(responses) = &operation.responses {
            for (status, response) in responses
                .statuses
                .iter()
                .map(|(status, response)| (status.as_str(), response))
                .chain(responses.default.as_ref().map(|it| ("default", it)))
            {
                if let Some(response) = self.resolved.response(response) {
                    let hint = extend(&extend(name, "response"), status);
                    self.response(response, &hint);
                }
            }
        }
    }

    fn parameters(
        &mut self,
        parameters: Option<&[MaybeRef<crate::doc::Parameter>]>,
        name: &[String],
    ) {
        for parameter in parameters.into_iter().flatten() {
            let Some(parameter) = self.resolved.parameter(parameter) else {
                continue;
            };
            // The parameter's own name, which is what a reader recognizes — the index in the array
            // is an accident of how the document was written.
            let hint = match &parameter.name {
                Some(wire) => extend(name, wire),
                None => name.to_vec(),
            };
            if let Some(id) = parameter.schema {
                self.push(id, hint.clone(), self.kind);
            }
            self.content(parameter.content.as_ref(), &hint);
        }
    }

    fn response(&mut self, response: &crate::doc::Response, name: &[String]) {
        self.content(response.content.as_ref(), name);
        for (header, node) in response.headers.iter().flatten() {
            if let Some(node) = self.resolved.header(node) {
                let hint = extend(name, header);
                if let Some(id) = node.schema {
                    self.push(id, hint.clone(), self.kind);
                }
                self.content(node.content.as_ref(), &hint);
            }
        }
    }

    fn content(
        &mut self,
        content: Option<&std::collections::BTreeMap<String, MediaType>>,
        name: &[String],
    ) {
        let Some(content) = content else {
            return;
        };
        // With one media type the name says nothing about it; with several it has to, or two
        // bodies of one operation would ask for the same name.
        let several = content.len() > 1;
        for (media_type, entry) in content {
            let Some(id) = entry.schema else {
                continue;
            };
            let hint = if several {
                extend(name, &media_suffix(media_type))
            } else {
                name.to_vec()
            };
            self.push(id, hint, self.kind);
        }
    }
}

fn extend(name: &[String], segment: &str) -> Vec<String> {
    let mut extended = name.to_vec();
    extended.push(segment.to_owned());
    extended
}

/// The name an operation contributes.
///
/// `operationId` when the document gives one, because that is the name the operation will have in
/// the generated client too; otherwise the method and the route, which is what the API model will
/// synthesize for the same operation.
fn operation_name(operation: &Operation, method: &str, route: &str) -> Vec<String> {
    if let Some(id) = &operation.id
        && !id.trim().is_empty()
    {
        return vec![id.clone()];
    }
    let mut segments = vec![method.to_owned()];
    segments.extend(route_segments(route));
    segments
}

/// A route template as name segments: `/pets/{petId}` becomes `["pets", "petId"]`.
fn route_segments(route: &str) -> Vec<String> {
    route
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.trim_matches(['{', '}']).to_owned())
        .collect()
}

/// The part of a media type worth putting in a name: `application/vnd.api+json` becomes `json`.
fn media_suffix(media_type: &str) -> String {
    let subtype = media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .rsplit('/')
        .next()
        .unwrap_or(media_type);
    subtype
        .rsplit('+')
        .next()
        .unwrap_or(subtype)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{media_suffix, route_segments};

    #[test]
    fn a_route_becomes_readable_name_segments() {
        assert_eq!(route_segments("/pets/{petId}"), ["pets", "petId"]);
        assert_eq!(route_segments("/"), [] as [String; 0]);
    }

    #[test]
    fn a_media_type_contributes_only_its_distinguishing_part() {
        assert_eq!(media_suffix("application/json"), "json");
        assert_eq!(media_suffix("application/vnd.api+json"), "json");
        assert_eq!(media_suffix("text/plain; charset=utf-8"), "plain");
        assert_eq!(media_suffix("multipart/form-data"), "form-data");
    }
}
