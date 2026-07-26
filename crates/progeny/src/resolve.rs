//! Reference resolution: `$ref` strings become ids, once, before any layer reads them.
//!
//! Parsing stores every reference exactly as written, so that a broken one is a finding rather
//! than a parse failure. This is the phase that turns those strings into addresses, and it is
//! deliberately the only phase that does: after it, no code path needs to know what a JSON
//! Pointer is to read a schema, and no code path can silently disagree with another about where
//! `#/components/schemas/Pet` points.
//!
//! It sits after both models rather than inside either, because it needs both: a schema `$ref`
//! resolves to a [`SchemaId`], and a document-level `$ref` resolves to a component of the
//! document tree. (The layout sketch in the plan filed it under `schema/`; the module-layer lint
//! is what caught that resolution cannot live to the left of the document model it reads.)
//!
//! **Scope, decided by measurement.** The corpus holds 0 external-file references, 0 `$dynamicRef`
//! and 0 `$anchor` across 535,105 schemas, and exactly one non-root `$id`. So external documents
//! are out of scope and diagnosed on use; `$dynamicRef` resolves as a plain `$ref`, which the
//! parser already reports; and anchors are resolved from a document-wide table rather than one
//! scoped per base URI. Each of those is a construct with no corpus caller, and building the
//! general machinery for it would be exactly the speculative generality this project deletes —
//! but each is *held* losslessly and behaves predictably, which is what keeps the choice reversible.

use std::collections::BTreeMap;

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::{
    Callback, Components, Document, Example, Header, MaybeRef, MediaType, Operation, Parameter,
    ParsedDocument, PathItem, Reference, RequestBody, Response,
};
use crate::schema::{Schema, SchemaId, SchemaStore, cycles::Sccs};

/// A document whose references have been resolved.
///
/// The type the layers after this one accept. Nothing constructs it but [`resolve`], so "the type
/// model never sees an unresolved reference" is a property of the signature rather than a rule
/// someone has to remember: there is no way to hand it a [`ParsedDocument`].
#[derive(Debug)]
pub(crate) struct ResolvedDocument {
    parsed: ParsedDocument,
    /// Where each schema's reference points. A schema with a reference and no entry here is one
    /// whose reference resolved to nothing, which the shape layer reads as "accept anything".
    targets: BTreeMap<SchemaId, SchemaId>,
    /// Each document-level reference string, mapped to the component name it ends up naming after
    /// any chain of references is followed.
    component_targets: BTreeMap<String, String>,
    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "read through `counts`, whose only caller is the feature-gated harness"
        )
    )]
    counts: Counts,
}

/// What resolution found, for the corpus to report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Schema references in total.
    pub references: usize,
    /// Schema references that resolved as written.
    pub resolved: usize,
    /// Schema references that resolved only after a repair.
    pub repaired: usize,
    /// Schema references that resolved to nothing.
    pub dangling: usize,
    /// Schema references addressing another document.
    pub external: usize,
    /// `$dynamicRef`s, resolved as though they were `$ref`s.
    pub dynamic: usize,
    /// Document-level references, to components rather than to schemas.
    pub component_references: usize,
    /// Document-level references that resolved to nothing.
    pub dangling_components: usize,
    /// Groups of schemas that reference each other, directly or through a chain.
    pub recursive_groups: usize,
    /// The size of the largest such group.
    pub largest_recursive_group: u32,
}

/// How deep a chain of references is followed before it is called a loop.
///
/// A chain longer than this is either a document doing something pathological or a cycle; either
/// way the answer is the same, and it is not worth distinguishing.
const MAX_CHAIN: usize = 32;

impl ResolvedDocument {
    pub(crate) fn document(&self) -> &Document {
        &self.parsed.document
    }

    pub(crate) fn schemas(&self) -> &SchemaStore {
        &self.parsed.schemas
    }

    pub(crate) fn schema(&self, id: SchemaId) -> &Schema {
        self.parsed.schemas.get(id)
    }

    /// Where this schema's reference points, if it has one that resolved.
    pub(crate) fn follow(&self, id: SchemaId) -> Option<SchemaId> {
        self.targets.get(&id).copied()
    }

    #[cfg_attr(
        not(feature = "harness"),
        allow(
            dead_code,
            reason = "the corpus reports these, and the corpus harness is feature-gated"
        )
    )]
    pub(crate) fn counts(&self) -> Counts {
        self.counts
    }

    /// The component name a document-level reference ends up naming.
    fn component_target(&self, node: &Reference) -> Option<&String> {
        self.component_targets.get(node.target.as_deref()?)
    }
}

/// Read a component through a reference, or read it directly.
///
/// One of these per referable node type. They are separate functions rather than one generic one
/// because each has to name its own section of `components`, and a generic version would have to
/// be handed the section by the caller — which is the mistake: the caller knowing which section a
/// `MaybeRef<Response>` lives in is how a reference to the wrong section becomes invisible.
macro_rules! component_reader {
    ($name:ident, $ty:ty, $section:ident) => {
        impl ResolvedDocument {
            pub(crate) fn $name<'a>(&'a self, node: &'a MaybeRef<$ty>) -> Option<&'a $ty> {
                match node {
                    MaybeRef::Item(item) => Some(item),
                    MaybeRef::Reference(reference) => {
                        let name = self.component_target(reference)?;
                        match self
                            .parsed
                            .document
                            .components
                            .as_ref()?
                            .$section
                            .as_ref()?
                            .get(name)?
                        {
                            MaybeRef::Item(item) => Some(item),
                            // The stored target is the end of the chain, so this cannot be
                            // another reference unless the chain looped — in which case there is
                            // no item to read and the diagnostic for it was already recorded.
                            MaybeRef::Reference(_) => None,
                        }
                    }
                }
            }
        }
    };
}

component_reader!(response, Response, responses);
component_reader!(parameter, Parameter, parameters);
component_reader!(header, Header, headers);
component_reader!(request_body, RequestBody, request_bodies);
component_reader!(path_item, PathItem, path_items);

/// Resolve every reference in the document.
pub(crate) fn resolve(parsed: ParsedDocument, ctx: &mut Ctx) -> ResolvedDocument {
    let index = Index::build(&parsed.schemas);
    let mut counts = Counts::default();
    let mut targets = BTreeMap::new();
    let mut edges = Vec::new();

    for (id, schema) in parsed.schemas.iter() {
        let Schema::Object(object) = schema else {
            continue;
        };
        object.children(|child| edges.push((id.raw(), child.raw())));

        // `$dynamicRef` is resolved as though it were `$ref`: the parser has already reported
        // that dynamic scoping is not implemented, so resolving it plainly is the behaviour that
        // diagnostic promises.
        let (written, dynamic) = match (&object.reference, &object.dynamic_reference) {
            (Some(reference), _) => (reference, false),
            (None, Some(reference)) => (reference, true),
            (None, None) => continue,
        };
        if dynamic {
            counts.dynamic += 1;
        }
        counts.references += 1;

        match index.resolve(written, &parsed.schemas) {
            Found::Exactly(target) => {
                counts.resolved += 1;
                targets.insert(id, target);
                edges.push((id.raw(), target.raw()));
            }
            Found::After(target, repair) => {
                counts.repaired += 1;
                targets.insert(id, target);
                edges.push((id.raw(), target.raw()));
                ctx.report(Diagnostic::new(
                    BreakageClass::DanglingRef,
                    Action::Repair,
                    parsed.schemas.address(id).child("$ref"),
                    format!(
                        "`$ref` does not address anything in this document; {repair}, so it was \
                         read as the schema that match names"
                    ),
                ));
            }
            Found::Nothing(reason) => {
                if reason == Reason::External {
                    counts.external += 1;
                } else {
                    counts.dangling += 1;
                }
                ctx.report(Diagnostic::new(
                    BreakageClass::DanglingRef,
                    Action::Degrade,
                    parsed.schemas.address(id).child("$ref"),
                    format!("{}; the schema accepts anything instead", reason.detail()),
                ));
            }
        }
    }

    let mut walk = Walk {
        components: parsed.document.components.as_ref(),
        targets: BTreeMap::new(),
        ctx,
        counts: &mut counts,
    };
    walk.document(&parsed.document);
    let component_targets = walk.targets;

    // Reported rather than consumed: how recursive a document is says something worth knowing about
    // it, but the cycles that *decide* anything are the ones in the generated type graph, and merging
    // and deduplication both reshape that. The contract layer runs the same routine over the graph
    // rustc will actually compile.
    let cycles = Sccs::of(parsed.schemas.len(), &edges);
    counts.recursive_groups = cycles.recursive_groups();
    counts.largest_recursive_group = cycles.largest_group();

    ResolvedDocument {
        parsed,
        targets,
        component_targets,
        counts,
    }
}

/// What resolving one reference found.
enum Found {
    /// The reference addressed a schema, as written.
    Exactly(SchemaId),
    /// It addressed one only after a repair, described by the second field.
    After(SchemaId, &'static str),
    Nothing(Reason),
}

/// Why a reference resolved to nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// It addresses a different document.
    External,
    /// It addresses a position in this document that holds no schema.
    Missing,
    /// It names an anchor that is declared more than once.
    Ambiguous,
    /// It is not a reference progeny can read at all.
    Unreadable,
}

impl Reason {
    fn detail(self) -> &'static str {
        match self {
            Self::External => {
                "`$ref` addresses another document, and progeny resolves references within one \
                 document only"
            }
            Self::Missing => "`$ref` addresses a position this document has no schema at",
            Self::Ambiguous => {
                "`$ref` names an anchor this document declares more than once, so it addresses no \
                 single schema"
            }
            Self::Unreadable => {
                "`$ref` is neither a JSON Pointer nor an anchor name, so there is nothing to \
                 resolve"
            }
        }
    }
}

/// Everything a reference can be resolved against, built once per document.
struct Index {
    /// Every schema by the position it was written at.
    addresses: BTreeMap<String, SchemaId>,
    /// Every `$id`, and whether it is unique.
    ids: BTreeMap<String, Option<SchemaId>>,
    /// Every `$anchor`, and whether it is unique.
    anchors: BTreeMap<String, Option<SchemaId>>,
}

impl Index {
    fn build(store: &SchemaStore) -> Self {
        let mut addresses = BTreeMap::new();
        let mut ids = BTreeMap::new();
        let mut anchors = BTreeMap::new();
        for (id, schema) in store.iter() {
            addresses.insert(store.address(id).to_string(), id);
            let Schema::Object(object) = schema else {
                continue;
            };
            if let Some(name) = &object.id {
                declare(&mut ids, name, id);
            }
            // A `$dynamicAnchor` is read as a plain `$anchor`, which is what the parser's
            // dynamic-scoping diagnostic says happens.
            for name in [&object.anchor, &object.dynamic_anchor]
                .into_iter()
                .flatten()
            {
                declare(&mut anchors, name, id);
            }
        }
        Self {
            addresses,
            ids,
            anchors,
        }
    }

    /// Resolve one reference string, repairing the two misspellings real documents make.
    fn resolve(&self, written: &str, store: &SchemaStore) -> Found {
        let reason = match self.direct(written, store) {
            Ok(id) => return Found::Exactly(id),
            Err(reason) => reason,
        };

        // A bare component name, with the `#/components/schemas/` prefix left off. Observed in
        // the wild often enough to be worth repairing rather than degrading.
        let bare = !written.contains('#') && !written.contains('/') && !written.is_empty();
        if bare {
            let guess = JsonPointer::root()
                .child("components")
                .child("schemas")
                .child(written);
            if let Some(&id) = self.addresses.get(&guess.to_string()) {
                return Found::After(id, "it names a schema component with the prefix left off");
            }
        }

        // A case variant of a real address: `#/components/schemas/pet` for `Pet`. Only for a
        // reference that means to address *this* document: another file's `Pet` and this one's
        // are different schemas, and "repairing" between them would resolve to the wrong schema, wearing
        // a diagnostic.
        if reason != Reason::External
            && let Some(fragment) = fragment_of(written)
            && let Some(pointer) = JsonPointer::parse(fragment)
            && let Folded::One(id) = self.folded_get(&pointer.to_string().to_lowercase())
        {
            return Found::After(id, "it differs from a real address only in letter case");
        }

        // A bare name that no repair rescued addresses nothing here, which is a more useful thing
        // to be told than that a file called `Pet` was not read.
        Found::Nothing(match (bare, reason) {
            (true, Reason::External) => Reason::Missing,
            (_, reason) => reason,
        })
    }

    fn direct(&self, written: &str, store: &SchemaStore) -> Result<SchemaId, Reason> {
        let (base, fragment) = split(written);
        let within = if base.is_empty() {
            None
        } else {
            // A base that names a `$id` in this document is not external, however absolute it
            // looks; anything else is another file, which is out of scope.
            match self.ids.get(base) {
                Some(Some(id)) => Some(*id),
                Some(None) => return Err(Reason::Ambiguous),
                None => return Err(Reason::External),
            }
        };

        let Some(fragment) = fragment else {
            // `{uri}` with no fragment addresses the schema that declared that `$id`.
            return within.ok_or(Reason::Unreadable);
        };
        if fragment.is_empty() || fragment.starts_with('/') {
            let Some(pointer) = JsonPointer::parse(fragment) else {
                return Err(Reason::Unreadable);
            };
            // A pointer is read from the document root even inside a nested `$id` scope, unless
            // the reference named that scope itself. Strict JSON Schema would resolve it against
            // the innermost base URI; every OpenAPI document and every tool that reads one means
            // the root, and the corpus has exactly one non-root `$id` to disagree about.
            let address = match within {
                Some(id) => join(store.address(id), &pointer),
                None => pointer,
            };
            return self
                .addresses
                .get(&address.to_string())
                .copied()
                .ok_or(Reason::Missing);
        }
        match self.anchors.get(fragment) {
            Some(Some(id)) => Ok(*id),
            Some(None) => Err(Reason::Ambiguous),
            None => Err(Reason::Missing),
        }
    }

    /// Look an address up with letter case ignored.
    ///
    /// A document with a broken reference pays for one pass over its own addresses; a document with
    /// none — 78 of 78 in the corpus — never reaches this at all, which is why there is no index to
    /// build up front.
    fn folded_get(&self, folded: &str) -> Folded {
        let mut found = Folded::Nothing;
        for (address, &id) in &self.addresses {
            if address.to_lowercase() == folded {
                found = match found {
                    Folded::Nothing => Folded::One(id),
                    Folded::One(_) | Folded::Ambiguous => Folded::Ambiguous,
                };
            }
        }
        found
    }
}

/// What a case-folded address lookup found.
///
/// A name that two addresses fold to addresses neither of them, so the third arm is not the same as
/// "not found": repairing to one of two candidates would be a guess wearing a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Folded {
    One(SchemaId),
    Ambiguous,
    Nothing,
}

/// Record a name that addresses a schema, or mark it ambiguous if it is declared twice.
///
/// A name declared twice addresses no single schema, so a reference to it degrades rather than
/// silently picking one of them — which would be the forbidden failure mode over a construct no
/// corpus document uses at all.
fn declare(table: &mut BTreeMap<String, Option<SchemaId>>, name: &str, id: SchemaId) {
    table
        .entry(name.to_owned())
        .and_modify(|slot| *slot = None)
        .or_insert(Some(id));
}

/// Split a reference into its base URI and its fragment.
fn split(written: &str) -> (&str, Option<&str>) {
    match written.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (written, None),
    }
}

fn fragment_of(written: &str) -> Option<&str> {
    split(written).1
}

/// Append one pointer to another, which is how a fragment is read inside a nested `$id`.
fn join(base: &JsonPointer, relative: &JsonPointer) -> JsonPointer {
    let mut joined = base.clone();
    for token in relative.tokens() {
        joined = joined.child(token.clone());
    }
    joined
}

/// The document walk that finds every reference to a component.
struct Walk<'a> {
    components: Option<&'a Components>,
    targets: BTreeMap<String, String>,
    ctx: &'a mut Ctx,
    counts: &'a mut Counts,
}

/// The sections of `components` a document-level reference can address.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Responses,
    Parameters,
    Headers,
    RequestBodies,
    Examples,
    Links,
    Callbacks,
    PathItems,
    SecuritySchemes,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Parameters => "parameters",
            Self::Headers => "headers",
            Self::RequestBodies => "requestBodies",
            Self::Examples => "examples",
            Self::Links => "links",
            Self::Callbacks => "callbacks",
            Self::PathItems => "pathItems",
            Self::SecuritySchemes => "securitySchemes",
        }
    }
}

/// One step along a chain of component references.
enum Step<'a> {
    /// The section holds the named component, and it is a component rather than a reference.
    Item,
    /// It holds another reference, to this target.
    Indirect(&'a str),
    /// It holds nothing under that name.
    Absent,
}

impl Walk<'_> {
    /// Resolve one reference to a component, recording where the chain ends.
    fn reference(&mut self, node: &Reference, section: Section, at: &JsonPointer) {
        *self.counts = Counts {
            component_references: self.counts.component_references + 1,
            ..*self.counts
        };
        let Some(written) = node.target.as_deref() else {
            return;
        };
        if self.targets.contains_key(written) {
            return;
        }
        if let Some(name) = self.chase(written, section) {
            self.targets.insert(written.to_owned(), name);
        } else {
            {
                self.counts.dangling_components += 1;
                self.ctx.report(Diagnostic::new(
                    BreakageClass::DanglingRef,
                    Action::Degrade,
                    at.child("$ref"),
                    format!(
                        "`$ref` does not address a component under `#/components/{}`, so nothing \
                         reads the node it stands for",
                        section.name()
                    ),
                ));
            }
        }
    }

    /// Follow a chain of component references to the name at its end.
    fn chase(&self, written: &str, section: Section) -> Option<String> {
        let mut at = written.to_owned();
        for _ in 0..MAX_CHAIN {
            let name = component_name(&at, section)?;
            match self.step(section, name) {
                Step::Item => return Some(name.to_owned()),
                Step::Indirect(next) => at = next.to_owned(),
                Step::Absent => return None,
            }
        }
        None
    }

    fn step(&self, section: Section, name: &str) -> Step<'_> {
        let Some(components) = self.components else {
            return Step::Absent;
        };
        match section {
            Section::Responses => step(components.responses.as_ref(), name),
            Section::Parameters => step(components.parameters.as_ref(), name),
            Section::Headers => step(components.headers.as_ref(), name),
            Section::RequestBodies => step(components.request_bodies.as_ref(), name),
            Section::Examples => step(components.examples.as_ref(), name),
            Section::Links => step(components.links.as_ref(), name),
            Section::Callbacks => step(components.callbacks.as_ref(), name),
            Section::PathItems => step(components.path_items.as_ref(), name),
            Section::SecuritySchemes => step(components.security_schemes.as_ref(), name),
        }
    }

    fn maybe_ref<T>(
        &mut self,
        node: &MaybeRef<T>,
        section: Section,
        at: &JsonPointer,
        item: impl FnOnce(&mut Self, &T, &JsonPointer),
    ) {
        match node {
            MaybeRef::Reference(reference) => self.reference(reference, section, at),
            MaybeRef::Item(inner) => item(self, inner, at),
        }
    }

    fn document(&mut self, document: &Document) {
        let root = JsonPointer::root();
        for (section, items) in [
            ("paths", document.paths.as_ref().map(|paths| &paths.items)),
            ("webhooks", document.webhooks.as_ref()),
        ] {
            let at = root.child(section);
            for (key, item) in items.into_iter().flatten() {
                let at = at.child(key.clone());
                self.maybe_ref(item, Section::PathItems, &at, Self::path_item);
            }
        }
        if let Some(components) = document.components.as_ref() {
            self.components(components);
        }
    }

    fn components(&mut self, components: &Components) {
        let at = JsonPointer::root().child("components");
        for (name, item) in components.responses.iter().flatten() {
            let at = at.child("responses").child(name.clone());
            self.maybe_ref(item, Section::Responses, &at, Self::response);
        }
        for (name, item) in components.parameters.iter().flatten() {
            let at = at.child("parameters").child(name.clone());
            self.maybe_ref(item, Section::Parameters, &at, Self::parameter);
        }
        for (name, item) in components.headers.iter().flatten() {
            let at = at.child("headers").child(name.clone());
            self.maybe_ref(item, Section::Headers, &at, Self::header);
        }
        for (name, item) in components.request_bodies.iter().flatten() {
            let at = at.child("requestBodies").child(name.clone());
            self.maybe_ref(item, Section::RequestBodies, &at, Self::request_body);
        }
        for (name, item) in components.callbacks.iter().flatten() {
            let at = at.child("callbacks").child(name.clone());
            self.maybe_ref(item, Section::Callbacks, &at, Self::callback);
        }
        for (name, item) in components.path_items.iter().flatten() {
            let at = at.child("pathItems").child(name.clone());
            self.maybe_ref(item, Section::PathItems, &at, Self::path_item);
        }
        for (name, item) in components.examples.iter().flatten() {
            let at = at.child("examples").child(name.clone());
            self.maybe_ref(item, Section::Examples, &at, |_, _, _| {});
        }
        for (name, item) in components.links.iter().flatten() {
            let at = at.child("links").child(name.clone());
            self.maybe_ref(item, Section::Links, &at, |_, _, _| {});
        }
        for (name, item) in components.security_schemes.iter().flatten() {
            let at = at.child("securitySchemes").child(name.clone());
            self.maybe_ref(item, Section::SecuritySchemes, &at, |_, _, _| {});
        }
    }

    fn path_item(&mut self, item: &PathItem, at: &JsonPointer) {
        self.parameters(item.parameters.as_deref(), at);
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
                self.operation(operation, &at.child(method));
            }
        }
    }

    fn operation(&mut self, operation: &Operation, at: &JsonPointer) {
        self.parameters(operation.parameters.as_deref(), at);
        if let Some(body) = &operation.request_body {
            let at = at.child("requestBody");
            self.maybe_ref(body, Section::RequestBodies, &at, Self::request_body);
        }
        if let Some(responses) = &operation.responses {
            let at = at.child("responses");
            for (status, response) in responses
                .statuses
                .iter()
                .map(|(status, response)| (status.as_str(), response))
                .chain(responses.default.as_ref().map(|it| ("default", it)))
            {
                let at = at.child(status);
                self.maybe_ref(response, Section::Responses, &at, Self::response);
            }
        }
        for (name, callback) in operation.callbacks.iter().flatten() {
            let at = at.child("callbacks").child(name.clone());
            self.maybe_ref(callback, Section::Callbacks, &at, Self::callback);
        }
    }

    fn parameters(&mut self, parameters: Option<&[MaybeRef<Parameter>]>, at: &JsonPointer) {
        let at = at.child("parameters");
        for (index, parameter) in parameters.into_iter().flatten().enumerate() {
            let at = at.child(index.to_string());
            self.maybe_ref(parameter, Section::Parameters, &at, Self::parameter);
        }
    }

    fn parameter(&mut self, parameter: &Parameter, at: &JsonPointer) {
        self.examples(parameter.examples.as_ref(), at);
        self.content(parameter.content.as_ref(), at);
    }

    fn header(&mut self, header: &Header, at: &JsonPointer) {
        self.examples(header.examples.as_ref(), at);
        self.content(header.content.as_ref(), at);
    }

    fn request_body(&mut self, body: &RequestBody, at: &JsonPointer) {
        self.content(body.content.as_ref(), at);
    }

    fn response(&mut self, response: &Response, at: &JsonPointer) {
        self.headers(response.headers.as_ref(), at);
        self.content(response.content.as_ref(), at);
        for (name, link) in response.links.iter().flatten() {
            let at = at.child("links").child(name.clone());
            self.maybe_ref(link, Section::Links, &at, |_, _, _| {});
        }
    }

    fn callback(&mut self, callback: &Callback, at: &JsonPointer) {
        for (expression, item) in &callback.expressions {
            let at = at.child(expression.clone());
            self.maybe_ref(item, Section::PathItems, &at, Self::path_item);
        }
    }

    fn headers(&mut self, headers: Option<&BTreeMap<String, MaybeRef<Header>>>, at: &JsonPointer) {
        let at = at.child("headers");
        for (name, header) in headers.into_iter().flatten() {
            let at = at.child(name.clone());
            self.maybe_ref(header, Section::Headers, &at, Self::header);
        }
    }

    fn examples(
        &mut self,
        examples: Option<&BTreeMap<String, MaybeRef<Example>>>,
        at: &JsonPointer,
    ) {
        let at = at.child("examples");
        for (name, example) in examples.into_iter().flatten() {
            let at = at.child(name.clone());
            self.maybe_ref(example, Section::Examples, &at, |_, _, _| {});
        }
    }

    fn content(&mut self, content: Option<&BTreeMap<String, MediaType>>, at: &JsonPointer) {
        let at = at.child("content");
        for (media_type, entry) in content.into_iter().flatten() {
            let at = at.child(media_type.clone());
            self.examples(entry.examples.as_ref(), &at);
            for (name, encoding) in entry.encoding.iter().flatten() {
                let at = at.child("encoding").child(name.clone());
                self.headers(encoding.headers.as_ref(), &at);
            }
        }
    }
}

/// Whether a section holds this name, and whether what it holds is another reference.
fn step<'a, T>(section: Option<&'a BTreeMap<String, MaybeRef<T>>>, name: &str) -> Step<'a> {
    match section.and_then(|entries| entries.get(name)) {
        Some(MaybeRef::Item(_)) => Step::Item,
        Some(MaybeRef::Reference(reference)) => match reference.target.as_deref() {
            Some(target) => Step::Indirect(target),
            None => Step::Absent,
        },
        None => Step::Absent,
    }
}

/// The component name a document-level reference addresses, if it addresses one at all.
///
/// Document-level references are `#/components/{section}/{name}` and nothing else: unlike a
/// schema, a Response Object has no addressable interior, so a pointer into one is not a
/// reference progeny can honour.
fn component_name(written: &str, section: Section) -> Option<&str> {
    let fragment = written.strip_prefix("#")?;
    let mut tokens = fragment.strip_prefix('/')?.split('/');
    if tokens.next()? != "components" || tokens.next()? != section.name() {
        return None;
    }
    let name = tokens.next()?;
    // A deeper pointer addresses something inside a component, which has no meaning here.
    if tokens.next().is_some() {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ResolvedDocument, resolve};
    use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};
    use crate::doc::parse as doc_parse;
    use crate::normalize;
    use crate::schema::Schema;

    fn resolve_document(document: Value) -> (ResolvedDocument, Vec<Diagnostic>) {
        let mut ctx = Ctx::new();
        let normalized = normalize::normalize(document, &mut ctx).unwrap();
        let parsed = doc_parse::document(normalized, &mut ctx);
        let resolved = resolve(parsed, &mut ctx);
        (resolved, ctx.into_diagnostics())
    }

    /// A document whose `components.schemas` are exactly these.
    fn with_schemas(schemas: Value) -> Value {
        // Built member by member rather than with `json!`, which would only borrow `schemas`.
        let mut components = serde_json::Map::new();
        components.insert("schemas".to_owned(), schemas);
        let mut root = serde_json::Map::new();
        root.insert("openapi".to_owned(), Value::String("3.1.0".to_owned()));
        root.insert("paths".to_owned(), Value::Object(serde_json::Map::new()));
        root.insert("components".to_owned(), Value::Object(components));
        Value::Object(root)
    }

    fn schema_named<'a>(resolved: &'a ResolvedDocument, name: &str) -> &'a Schema {
        let id = resolved
            .document()
            .components
            .as_ref()
            .and_then(|components| components.schemas.as_ref())
            .and_then(|schemas| schemas.get(name))
            .copied()
            .unwrap();
        resolved.schema(id)
    }

    fn id_of(resolved: &ResolvedDocument, name: &str) -> crate::schema::SchemaId {
        resolved
            .document()
            .components
            .as_ref()
            .and_then(|components| components.schemas.as_ref())
            .and_then(|schemas| schemas.get(name))
            .copied()
            .unwrap()
    }

    #[test]
    fn a_pointer_into_this_document_resolves() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "Pet": {"type": "object", "properties": {"friend": {"$ref": "#/components/schemas/Pet"}}},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let counts = resolved.counts();
        assert_eq!(counts.references, 1);
        assert_eq!(counts.resolved, 1);
        assert_eq!(counts.dangling, 0);

        let pet = id_of(&resolved, "Pet");
        let friend = match schema_named(&resolved, "Pet") {
            Schema::Object(object) => object.properties.as_ref().unwrap()["friend"],
            Schema::Bool(_) => panic!("expected an object schema"),
        };
        assert_eq!(resolved.follow(friend), Some(pet));
    }

    #[test]
    fn a_cycle_through_a_reference_is_counted() {
        let (resolved, _) = resolve_document(with_schemas(json!({
            "A": {"properties": {"b": {"$ref": "#/components/schemas/B"}}},
            "B": {"properties": {"a": {"$ref": "#/components/schemas/A"}}},
            "C": {"type": "string"},
        })));
        assert_eq!(resolved.counts().recursive_groups, 1);
        assert_eq!(resolved.counts().largest_recursive_group, 4);
    }

    #[test]
    fn a_chain_of_references_resolves_link_by_link() {
        let (resolved, _) = resolve_document(with_schemas(json!({
            "A": {"$ref": "#/components/schemas/B"},
            "B": {"$ref": "#/components/schemas/C"},
            "C": {"type": "string"},
        })));
        let b = resolved.follow(id_of(&resolved, "A")).unwrap();
        assert_eq!(b, id_of(&resolved, "B"));
        assert_eq!(resolved.follow(b), Some(id_of(&resolved, "C")));
    }

    #[test]
    fn a_pointer_into_a_subschema_resolves() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "Pet": {"properties": {"tag": {"type": "string"}}},
            "Tag": {"$ref": "#/components/schemas/Pet/properties/tag"},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let target = resolved.follow(id_of(&resolved, "Tag")).unwrap();
        assert!(matches!(
            resolved.schema(target),
            Schema::Object(object) if object.types.is_some()
        ));
    }

    #[test]
    fn the_two_observed_misspellings_are_repaired() {
        for (written,) in [("Pet",), ("#/components/schemas/pet",)] {
            let (resolved, diagnostics) = resolve_document(with_schemas(json!({
                "Pet": {"type": "object"},
                "Other": {"$ref": written},
            })));
            assert_eq!(resolved.counts().repaired, 1, "{written}");
            assert_eq!(
                resolved.follow(id_of(&resolved, "Other")),
                Some(id_of(&resolved, "Pet")),
                "{written}"
            );
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].class(), BreakageClass::DanglingRef);
            assert_eq!(diagnostics[0].action(), Action::Repair);
        }
    }

    #[test]
    fn a_reference_to_nothing_degrades_and_says_so() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "A": {"$ref": "#/components/schemas/Nope"},
        })));
        assert_eq!(resolved.counts().dangling, 1);
        assert_eq!(resolved.follow(id_of(&resolved, "A")), None);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].action(), Action::Degrade);
        assert!(diagnostics[0].detail().contains("accepts anything"));
    }

    #[test]
    fn a_reference_to_another_file_is_out_of_scope_and_counted_separately() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "A": {"$ref": "common.yaml#/components/schemas/Pet"},
        })));
        assert_eq!(resolved.counts().external, 1);
        assert_eq!(resolved.counts().dangling, 0);
        assert!(diagnostics[0].detail().contains("another document"));
    }

    #[test]
    fn a_nested_id_becomes_a_base_for_the_references_that_name_it() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "Pet": {
                "$id": "https://example.test/pet",
                "properties": {"tag": {"type": "string"}},
            },
            "Tag": {"$ref": "https://example.test/pet#/properties/tag"},
            "Whole": {"$ref": "https://example.test/pet"},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(resolved.counts().resolved, 2);
        assert_eq!(
            resolved.follow(id_of(&resolved, "Whole")),
            Some(id_of(&resolved, "Pet"))
        );
        let tag = resolved.follow(id_of(&resolved, "Tag")).unwrap();
        assert_eq!(
            resolved.schemas().address(tag).to_string(),
            "/components/schemas/Pet/properties/tag"
        );
    }

    #[test]
    fn an_anchor_resolves_and_a_duplicate_one_does_not() {
        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "Pet": {"$anchor": "pet", "type": "object"},
            "A": {"$ref": "#pet"},
        })));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            resolved.follow(id_of(&resolved, "A")),
            Some(id_of(&resolved, "Pet"))
        );

        let (resolved, diagnostics) = resolve_document(with_schemas(json!({
            "Pet": {"$anchor": "twice", "type": "object"},
            "Dog": {"$anchor": "twice", "type": "object"},
            "A": {"$ref": "#twice"},
        })));
        assert_eq!(resolved.counts().dangling, 1);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("more than once"))
        );
    }

    #[test]
    fn a_dynamic_reference_resolves_as_a_plain_one() {
        let (resolved, _) = resolve_document(with_schemas(json!({
            "Pet": {"$dynamicAnchor": "node", "type": "object"},
            "A": {"$dynamicRef": "#node"},
        })));
        assert_eq!(resolved.counts().dynamic, 1);
        assert_eq!(
            resolved.follow(id_of(&resolved, "A")),
            Some(id_of(&resolved, "Pet"))
        );
    }

    #[test]
    fn a_component_reference_resolves_through_a_chain() {
        let (resolved, diagnostics) = resolve_document(json!({
            "openapi": "3.1.0",
            "paths": {
                "/a": {"get": {"responses": {"200": {"$ref": "#/components/responses/Alias"}}}},
            },
            "components": {
                "responses": {
                    "Alias": {"$ref": "#/components/responses/Real"},
                    "Real": {"description": "the real one"},
                },
            },
        }));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(resolved.counts().component_references, 2);
        let responses = resolved
            .document()
            .paths
            .as_ref()
            .unwrap()
            .items
            .get("/a")
            .unwrap();
        let crate::doc::MaybeRef::Item(path) = responses else {
            panic!("expected a path item")
        };
        let arm = path
            .get
            .as_ref()
            .unwrap()
            .responses
            .as_ref()
            .unwrap()
            .statuses
            .get("200")
            .unwrap();
        assert_eq!(
            resolved
                .response(arm)
                .and_then(|it| it.description.as_deref()),
            Some("the real one")
        );
    }

    #[test]
    fn a_component_reference_to_nothing_is_reported() {
        let (resolved, diagnostics) = resolve_document(json!({
            "openapi": "3.1.0",
            "paths": {
                "/a": {"get": {"responses": {"200": {"$ref": "#/components/responses/Nope"}}}},
            },
        }));
        assert_eq!(resolved.counts().dangling_components, 1);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].detail().contains("#/components/responses"));
    }

    #[test]
    fn a_component_reference_into_the_wrong_section_does_not_resolve() {
        let (resolved, _) = resolve_document(json!({
            "openapi": "3.1.0",
            "paths": {
                "/a": {"get": {"responses": {"200": {"$ref": "#/components/schemas/Pet"}}}},
            },
            "components": {"schemas": {"Pet": {"type": "object"}}},
        }));
        // A Response Object cannot be a schema, so this is a dangling reference and not a
        // silently-wrong one.
        assert_eq!(resolved.counts().dangling_components, 1);
    }
}
