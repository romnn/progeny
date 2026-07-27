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

use std::collections::BTreeMap;

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
    // One router per method, because a route is only ambiguous against another route on the same
    // method — `axum` builds its own tree per method for exactly this reason, and sharing one here
    // would report `GET /pets/{id}` as colliding with `DELETE /pets/{name}`.
    let mut routers: BTreeMap<Method, matchit::Router<String>> = BTreeMap::new();
    for operation in operations {
        let path = operation.path.to_string();
        let router = routers.entry(operation.method).or_default();
        match router.insert(&path, path.clone()) {
            Ok(()) => {
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
                ctx.report(Diagnostic::new(
                    BreakageClass::UnregistrableRoute,
                    Action::Degrade,
                    operation.origin.clone(),
                    format!(
                        "the path is not one the router accepts (`matchit`: {other}); the \
                         generated server omits the operation and the client keeps it"
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Method, OperationContract, RegistrableRoute, classify};
    use crate::api::{ResponseContract, route};
    use crate::contract::RustIdent;
    use crate::diag::{Ctx, JsonPointer};
    use crate::shape::Docs;

    fn operation(method: Method, template: &str) -> OperationContract {
        OperationContract {
            rust_name: RustIdent::method(&[template.to_owned()]),
            method,
            path: route::parse(template).expect("the template parses"),
            params: Vec::new(),
            body: None,
            responses: ResponseContract {
                arms: Vec::new(),
                default: None,
            },
            docs: Docs::default(),
            registrable: None,
            pagination: None,
            origin: JsonPointer::root().child(template),
        }
    }

    /// What `classify` does with each of the three answers a router can give.
    ///
    /// The `matchit` tests below pin what the *router* says; this pins what progeny does about it,
    /// which is the part that can regress. Note the third case: two operations of the same shape on
    /// *different methods* are not a collision, because a router keeps a tree per method.
    #[test]
    fn a_refusal_and_a_collision_cost_the_handler_and_nothing_else() {
        let mut operations = vec![
            operation(Method::Get, "/pets/{id}"),
            operation(Method::Get, "/pets/{name}"),
            operation(Method::Get, "/thumbs/{id}.png"),
            operation(Method::Delete, "/pets/{other}"),
        ];
        let mut ctx = Ctx::new();
        classify(&mut operations, &mut ctx);

        let kept: Vec<bool> = operations
            .iter()
            .map(|operation| operation.registrable.is_some())
            .collect();
        assert_eq!(kept, [true, false, false, true]);
        assert_eq!(
            operations[0]
                .registrable
                .as_ref()
                .map(RegistrableRoute::path),
            Some("/pets/{id}")
        );

        let found = ctx.into_diagnostics();
        assert_eq!(found.len(), 2, "{found:#?}");
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
    }

    /// The rules this module refuses to model, pinned as observations rather than as beliefs.
    ///
    /// Not a test of progeny: a test of what progeny is asking. If a `matchit` upgrade changes any
    /// of these, the diagnostics change with it and the reviewer should see why.
    fn accepts(route: &str) -> bool {
        matchit::Router::new().insert(route, ()).is_ok()
    }

    fn accepts_beside(first: &str, second: &str) -> bool {
        let mut router = matchit::Router::new();
        router.insert(first, ()).expect("the first inserts");
        router.insert(second, ()).is_ok()
    }

    #[test]
    fn two_routes_of_the_same_shape_are_one_route() {
        // Two variables in the same position are the same route however they are named, which is
        // the whole of `miro`'s and `polygon`'s collisions: both vendors disambiguate a path by
        // renaming its parameter, which changes the documentation and not the URL.
        assert!(!accepts_beside("/a/{x}", "/a/{y}"));
        assert!(!accepts_beside(
            "/v1/ema/{cryptoTicker}",
            "/v1/ema/{fxTicker}"
        ));
        assert!(!accepts_beside("/a/{x}/b", "/a/{y}/b"));
        // A literal beats a variable rather than colliding with it.
        assert!(accepts_beside("/a/{x}", "/a/me"));
        // And differing later in the path is enough to tell two routes apart.
        assert!(accepts_beside("/a/{x}/b", "/a/{y}/c"));
    }

    #[test]
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
    /// progeny asks *its* `matchit` and the generated crate registers with *`axum`'s*. If those are
    /// ever different majors the answer progeny gives is about a router nobody runs, and the
    /// failure mode is a server that panics at startup — the one thing the classifier exists to
    /// make impossible. `axum::Router` is built here purely so cargo resolves axum's own matchit;
    /// a mismatch shows up as a duplicate-crate build rather than as a runtime surprise.
    #[test]
    fn the_router_progeny_asks_is_the_router_axum_uses() {
        let _: axum::Router<()> = axum::Router::new();
        let versions = std::process::Command::new("cargo")
            .args([
                "tree",
                "--package",
                "progeny",
                "--invert",
                "matchit",
                "--edges",
                "normal",
            ])
            .output();
        let Ok(output) = versions else {
            // No cargo on the path is a broken environment, not a failing invariant.
            return;
        };
        let tree = String::from_utf8_lossy(&output.stdout);
        let majors: std::collections::BTreeSet<&str> = tree
            .lines()
            .filter_map(|line| {
                line.trim_start_matches(['├', '─', '│', '└', ' '])
                    .strip_prefix("matchit v")
            })
            .filter_map(|version| version.split('.').next())
            .collect();
        assert!(
            majors.len() <= 1,
            "progeny and axum resolve different major versions of matchit, so the classifier is \
             answering about a router the generated server does not use: {majors:?}\n{tree}"
        );
    }
}
