//! Which routes a router can actually register.
//!
//! The other half of `unregistrable-route`, and a different question from the one
//! [`super::route`] answers. Whether a template can be *filled* is a property of one operation and
//! has been live since stage 5. Whether two templates *collide* is a property of a whole router,
//! so it could not be asked before there was one.
//!
//! **The rule is asked, not modelled**, and the corpus is why. `matchit` — the router `axum`
//! matches with — accepts `/Videos/{itemId}/stream.{container}` and refuses
//! `/Videos/{itemId}/Trickplay/{width}/{index}.jpg`: a parameter may have literal text before it in
//! its segment, may have none after it, and may not share the segment with another. None of that
//! follows from anything OpenAPI says, and none of it is what the client's fill rule does. It is
//! also **version-sensitive** — a later `matchit` relaxes the suffix rule — which is the strongest
//! argument of all against writing progeny's own copy of it: a model would be a claim about a
//! router nobody is running. So each template goes into a real router and the answer is whatever it
//! says.
//!
//! Being conservative is the safe direction here and it is still visible. If a consumer resolves a
//! `matchit` that accepts more than the one progeny asked, progeny will have omitted a route that
//! would have worked — and said so, by name, in a diagnostic. The other direction is a server that
//! panics at startup, which is the failure this exists to prevent.
//!
//! *Enforcement:* [`RegistrableRoute`] has no public constructor. The server renderer takes one, so
//! a route the router refused cannot reach rendering — the invariant is carried by a type rather
//! than by a check somebody has to remember to run.

use std::collections::BTreeSet;

use super::{Method, OperationContract};
use crate::diag::{Action, BreakageClass, Ctx, Diagnostic};

/// A route a router accepted. Only [`classify`] builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistrableRoute {
    /// The template as the router takes it, which is also how it was written: `axum` 0.8 and
    /// OpenAPI spell a path variable the same way, so there is nothing to translate.
    path: String,
    method: Method,
}

impl RegistrableRoute {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn method(&self) -> Method {
        self.method
    }
}

/// Decide which operations a router can serve, in document order.
///
/// Document order is what decides the winner of a collision, and it is the only order available: the
/// document says nothing about which of two colliding routes it meant, and picking by any other
/// property — length, specificity, alphabetical — would be progeny inventing an intent. First
/// written, first registered; every later one says what it lost to.
pub(crate) fn classify(operations: &mut [OperationContract], ctx: &mut Ctx) {
    // **One router for every method**, because that is what `axum` has: its `Router` keeps a single
    // path tree, and dispatching on the method happens inside a matched node. The first version of
    // this function kept a router per method on the belief that a route is only ambiguous against
    // another route on the same method — a model, and a wrong one, which the wire probe demolished
    // on its second document: `jellyfin`'s `GET …/Subtitles/{language}` and
    // `POST …/Subtitles/{subtitleId}` pass a per-method classifier and then panic the real router
    // at startup, the exact failure this module exists to make impossible. Asked-not-modelled
    // applies to the router's *sharding* too.
    //
    // Two operations on the *identical* template are fine and common — `GET` and `DELETE` of
    // `/pets/{id}` — because the server renderer merges them into one registration; only the first
    // occurrence of a template is inserted.
    let mut router = matchit::Router::new();
    let mut templates: BTreeSet<String> = BTreeSet::new();
    for operation in operations {
        // A body variant is its primary's route under another media type; the primary carries the
        // registration, and giving the variant one too would register the same route twice.
        if operation.body_variant {
            continue;
        }
        let path = operation.path.to_string();
        if templates.contains(&path) {
            operation.registrable = Some(RegistrableRoute {
                path,
                method: operation.method,
            });
            continue;
        }
        // axum validates on top of matchit before inserting: `Router::route` panics at startup
        // for any segment starting with `:` or `*` — the 0.7 capture spellings, common in
        // documents converted from Express-style routing — while matchit itself accepts both
        // as literals. Asked-not-modelled cannot reach that check (axum is a dev-dependency
        // here on purpose), so its two rules are replicated; the dev-dependency parity test
        // exercises the real `Router::route` against both spellings to keep the copy honest.
        let v07_spelling = path
            .split('/')
            .any(|segment| segment.starts_with(':') || segment.starts_with('*'));
        if v07_spelling {
            ctx.report(Diagnostic::new(
                BreakageClass::UnregistrableRoute,
                Action::Degrade,
                operation.origin.clone(),
                format!(
                    "`{} {path}` has a segment starting with `:` or `*`, which axum refuses at \
                     startup (its 0.7 capture spellings); the generated server omits this \
                     operation and the client keeps it",
                    operation.method.slug().to_uppercase()
                ),
            ));
            continue;
        }
        match router.insert(&path, path.clone()) {
            Ok(()) => {
                templates.insert(path.clone());
                operation.registrable = Some(RegistrableRoute {
                    path,
                    method: operation.method,
                });
            }
            Err(matchit::InsertError::Conflict { with }) => {
                // The operation is kept. A colliding route is perfectly callable — the client
                // builds a URL and sends it — and it is only the *router* that cannot tell two of
                // them apart. Skipping the operation outright would take a working client method
                // away to fix a server's problem.
                ctx.report(Diagnostic::new(
                    BreakageClass::UnregistrableRoute,
                    Action::Degrade,
                    operation.origin.clone(),
                    format!(
                        "`{} {path}` cannot be registered beside `{with}`, which the router \
                         already matches; the generated server omits this operation and the \
                         client keeps it",
                        operation.method.slug().to_uppercase()
                    ),
                ));
            }
            // The sentence names the router's reason and **not the route**, so that a document with
            // a habit folds into one record instead of a hundred. `twilio-api-v2010` puts `.json`
            // after a path variable in 99 operations and `anthropic` writes `?beta=true` into 41:
            // one finding each, and a record per occurrence is the failure mode the aggregation
            // rule exists to prevent. Which operations lost their handler is in the record's
            // locations, and in the generated source.
            //
            // The reason is **attributed** rather than asserted, because `matchit`'s is not always
            // accurate about the path it refused: `/screenshots/{scan_id}.png` has exactly one
            // parameter in every segment and still comes back as "Only one parameter is allowed per
            // path segment", that being its blanket message for a parameter that does not end its
            // segment. Naming the crate keeps a confusing message the router's own rather than
            // progeny's claim about the document — which matters more here than anywhere, because
            // this module deliberately does not model the rule well enough to say it better.
            Err(other) => {
                ctx.report(
                    Diagnostic::new(
                        BreakageClass::UnregistrableRoute,
                        Action::Degrade,
                        operation.origin.clone(),
                        format!(
                            "the path is not one the router accepts (`matchit`: {other}); the \
                             generated server omits the operation and the client keeps it"
                        ),
                    )
                    // Folded on a stable key rather than on the sentence, because the sentence
                    // quotes `matchit` and has already been reworded once: identity is "the router
                    // refused it", however the refusal is phrased this year.
                    .folded_as("router-refusal"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{Method, OperationContract, RegistrableRoute, classify};
    use crate::api::{ResponseContract, route};
    use crate::contract::RustIdent;
    use crate::diag::{Ctx, JsonPointer};
    use crate::shape::Docs;

    fn operation(method: Method, template: &str) -> eyre::Result<OperationContract> {
        Ok(OperationContract {
            body_variant: false,
            rust_name: RustIdent::method(&[template.to_owned()]),
            method,
            path: route::parse(template)?,
            params: Vec::new(),
            body: None,
            responses: ResponseContract {
                arms: Vec::new(),
                default: None,
                accept: Vec::new(),
            },
            docs: Docs::default(),
            registrable: None,
            pagination: None,
            origin: JsonPointer::root().child(template),
        })
    }

    /// What `classify` does with each of the answers a router can give.
    ///
    /// The `matchit` tests below pin what the *router* says; this pins what progeny does about it,
    /// which is the part that can regress. The last two cases are the ones the wire probe corrected:
    /// a conflicting shape loses its handler **whatever its method**, because `axum` keeps one path
    /// tree for all methods — while a second method on the *identical* template is the ordinary
    /// merged registration and keeps everything.
    #[test_util::test]
    fn a_refusal_and_a_collision_cost_the_handler_and_nothing_else() {
        let mut operations = vec![
            operation(Method::Get, "/pets/{id}")?,
            operation(Method::Get, "/pets/{name}")?,
            operation(Method::Get, "/thumbs/{id}.png")?,
            // The shape the first classifier waved through and the real router panicked on:
            // a conflicting template on a *different* method.
            operation(Method::Delete, "/pets/{other}")?,
            // And the case that must keep working: another method on the identical template.
            operation(Method::Delete, "/pets/{id}")?,
        ];
        let mut ctx = Ctx::new();
        classify(&mut operations, &mut ctx);

        let kept: Vec<bool> = operations
            .iter()
            .map(|operation| operation.registrable.is_some())
            .collect();
        assert_eq!(kept, [true, false, false, false, true]);
        assert_eq!(
            operations[0]
                .registrable
                .as_ref()
                .map(RegistrableRoute::path),
            Some("/pets/{id}")
        );

        let found = ctx.into_diagnostics();
        assert_eq!(found.len(), 3, "{found:#?}");
        // A collision names what it collided with, so a reader can find the winner.
        assert!(
            found[0].detail().contains("`/pets/{id}`"),
            "{}",
            found[0].detail()
        );
        // A refusal attributes the reason rather than asserting it: `matchit` answers this one with
        // its blanket per-segment message even though the path has one parameter per segment.
        assert!(
            found[1].detail().contains("`matchit`:"),
            "{}",
            found[1].detail()
        );
        // The cross-method collision reads like any other collision.
        assert!(
            found[2].detail().contains("`DELETE /pets/{other}`"),
            "{}",
            found[2].detail()
        );
    }

    /// The rules this module refuses to model, pinned as observations rather than as beliefs.
    ///
    /// Not a test of progeny: a test of what progeny is asking. If a `matchit` upgrade changes any
    /// of these, the diagnostics change with it and the reviewer should see why.
    fn accepts(route: &str) -> bool {
        matchit::Router::new().insert(route, ()).is_ok()
    }

    fn accepts_beside(first: &str, second: &str) -> eyre::Result<bool> {
        let mut router = matchit::Router::new();
        router.insert(first, ())?;
        Ok(router.insert(second, ()).is_ok())
    }

    #[test_util::test]
    fn two_routes_of_the_same_shape_are_one_route() {
        // Two variables in the same position are the same route however they are named, which is
        // the whole of `miro`'s and `polygon`'s collisions: both vendors disambiguate a path by
        // renaming its parameter, which changes the documentation and not the URL.
        assert!(!accepts_beside("/a/{x}", "/a/{y}")?);
        assert!(!accepts_beside(
            "/v1/ema/{cryptoTicker}",
            "/v1/ema/{fxTicker}"
        )?);
        assert!(!accepts_beside("/a/{x}/b", "/a/{y}/b")?);
        // A literal beats a variable rather than colliding with it.
        assert!(accepts_beside("/a/{x}", "/a/me")?);
        // And differing later in the path is enough to tell two routes apart.
        assert!(accepts_beside("/a/{x}/b", "/a/{y}/c")?);
    }

    #[test_util::test]
    fn a_parameter_has_to_end_its_segment_and_be_the_only_one_in_it() {
        // The rule this module exists to stop progeny from guessing at. A parameter may have
        // literal text *before* it in the segment and may not have any after it, and a segment may
        // hold only one — none of which follows from anything in OpenAPI, and all of which the
        // client's own fill rule happily allows.
        assert!(accepts("/Videos/{itemId}/stream.{container}"));
        assert!(accepts("/s.{a}"));
        assert!(!accepts("/a/{x}.jpg"));
        assert!(!accepts("/{a}x{b}"));
        assert!(!accepts("/Videos/{itemId}/Trickplay/{width}/{index}.jpg"));
        assert!(accepts("/"));
        assert!(accepts("/a/{x}/"));
    }

    /// The coupling this module is built on, asserted rather than assumed.
    ///
    /// progeny asks *its* `matchit` and the generated crate registers with *axum's*. If those
    /// ever resolve to incompatible versions the answer progeny gives is about a router nobody
    /// runs, and the failure mode is a server that panics at startup — the one thing the
    /// classifier exists to make impossible. `axum::Router` is built here so the dependency this
    /// test is about actually exists in the graph.
    ///
    /// Read from the checked-in lockfile rather than by shelling out to `cargo tree`, for two
    /// reasons that both bit the previous version. A subprocess gave the test a path where it
    /// silently passed — no `cargo` on `PATH` meant no assertion — and a test that can pass by not
    /// running enforces nothing. And comparing *major* numbers was wrong for a `0.x` crate:
    /// matchit 0.8 and 0.9 are semver-incompatible and both have major `0`, so the exact drift
    /// being guarded against would have passed. Counting lockfile entries delegates compatibility
    /// to cargo itself: it unifies versions within one compatible range, so a second `matchit`
    /// entry appears exactly when progeny and axum stop sharing one router.
    #[test_util::test]
    fn a_v07_capture_spelling_is_degraded_not_a_startup_panic() {
        // `/repos/:owner` and `/files/*path` — what a Swagger/Express conversion writes.
        // matchit takes both as literals, so the bare-router question misses them, and the
        // panic would land at the consumer's server startup.
        let (model, diagnostics) =
            crate::api::tests::model_of(crate::api::tests::with_paths(serde_json::json!({
                "/repos/:owner": {"get": {
                    "operationId": "byOwner",
                    "responses": {"204": {"description": "done"}},
                }},
                "/plain": {"get": {
                    "operationId": "plain",
                    "responses": {"204": {"description": "done"}},
                }},
            })))?;
        let registrable: Vec<&str> = model
            .operations()
            .iter()
            .filter(|operation| operation.registrable.is_some())
            .map(|operation| operation.rust_name.as_str())
            .collect();
        assert_eq!(registrable, ["plain"], "{registrable:?}");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail().contains("axum refuses at startup")),
            "{diagnostics:?}"
        );
        // The replicated rule stays honest against the real router: axum's own `route` panics
        // for exactly these spellings.
        for path in ["/repos/:owner", "/files/*path"] {
            let refused = std::panic::catch_unwind(|| {
                let _: axum::Router<()> =
                    axum::Router::new().route(path, axum::routing::get(|| async {}));
            });
            assert!(refused.is_err(), "axum now accepts `{path}`");
        }
    }

    #[test_util::test]
    fn the_router_progeny_asks_is_the_router_axum_uses() {
        let _: axum::Router<()> = axum::Router::new();
        // `include_str!` rather than a runtime read, for two reasons: the resolved dependency
        // graph is a compile-time fact, so compile time is when to capture it; and the library's
        // no-I/O rule is mechanical — `lint-layers` rejects `std::fs` here, tests included.
        let lockfile = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"));
        let resolved: Vec<&str> = lockfile
            .lines()
            .zip(lockfile.lines().skip(1))
            .filter(|(name, _)| *name == "name = \"matchit\"")
            .map(|(_, version)| version)
            .collect();
        assert_eq!(
            resolved.len(),
            1,
            "cargo resolved more than one `matchit`, so progeny and axum are not asking the same \
             router and the classifier's answers are about one the generated server does not use: \
             {resolved:?}"
        );
    }
}
