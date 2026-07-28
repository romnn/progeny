//! The front end, exposed for the conformance corpus runner and the fuzz targets.
//!
//! **Not public API.** Behind the non-default `harness` feature, `#[doc(hidden)]`, and exempt
//! from semver. It exists because the round-trip property is the central claim this project rests
//! on, and a claim that only a private test can check is a claim nobody can re-run. Everything
//! here is a question about a document, never a way to reach into the model: exposing the model
//! itself would recreate the general-purpose-library surface progeny exists not to have.
//!
//! One module per question the corpus asks: [`trip`] whether the model holds the document,
//! [`convergence`] whether the two dialects generate the same crate, [`payload`] what the
//! examples oblige the generated types to do, [`probe`] what a live round trip through a
//! generated client and server must answer, and [`stats`] what the corpus contains.

mod convergence;
mod payload;
mod probe;
mod stats;
mod trip;

use crate::diag::{Ctx, Diagnostic, RejectError};
use crate::{doc, load, normalize, resolve};

pub use crate::resolve::Counts as Resolution;
pub use convergence::{Convergence, convergence};
pub use payload::{Payload, Payloads, payloads};
pub use probe::{
    Probe, ProbeAnswer, ProbeBody, ProbeGroup, ProbeOp, ProbeResponse, ProbeSetter, probe,
};
pub use stats::{AnyOfShapes, Stats, stats};
pub use trip::{RoundTrip, round_trip};

/// How many derives every generated type carries whatever the configuration says.
///
/// The differential harness subtracts these from its per-type body counts so the remaining
/// number is about serde; it once kept its own `2.0`, which a third always-emitted derive would
/// have silently made a lie in the project's headline ratio.
#[must_use]
pub fn base_derive_count() -> usize {
    crate::contract::BASE_DERIVES.len()
}

/// One place where the model did not hold what the document said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Where in the document.
    pub location: String,
    /// What the difference is, as a short human sentence.
    pub detail: String,
}

/// Make a caught panic stop being fatal, for the fuzz targets.
///
/// The loader deliberately contains a panic boundary: `libyaml-safer` is a port of a C library and
/// panics on a few adversarial inputs, and progeny turns that into an ordinary rejection because "no
/// input panics the generator" is the invariant that matters. `libfuzzer-sys` installs a panic hook
/// that **aborts**, and a hook runs before unwinding — so under a fuzzer the boundary never gets to
/// act, and every such input is reported as a crash.
///
/// This replaces the hook with one that reports and returns. A panic the library catches then costs
/// a line on stderr; a panic that *escapes* still unwinds out of the fuzz target's `extern "C"`
/// entry point and still aborts, so the property under test is unchanged.
///
/// Only a fuzz target should call this. A library has no business touching a process-wide hook, and
/// this one is behind the same feature gate and the same no-semver promise as the rest of the module.
pub fn allow_caught_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            eprintln!("progeny: caught a panic and turned it into a rejection: {info}");
        }));
    });
}

/// Resolve a document's references and report the accounting.
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn resolution(input: &[u8]) -> Result<Resolution, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    Ok(resolve::resolve(parsed, &mut ctx).counts())
}

/// Run the front end for its diagnostics only.
///
/// The shape a fuzz target wants: it exercises every code path from bytes to model and discards
/// the result, so the property under test is "no input panics".
///
/// # Errors
///
/// Returns [`RejectError`] when the document is unusable.
pub fn front_end(input: &[u8]) -> Result<Vec<Diagnostic>, RejectError> {
    let mut ctx = Ctx::new();
    let loaded = load::load(input, &mut ctx)?;
    let normalized = normalize::normalize(loaded.value, &mut ctx)?;
    let parsed = doc::parse::document(normalized, &mut ctx);
    let _ = doc::serialize::document(&parsed);
    let _ = resolve::resolve(parsed, &mut ctx);
    Ok(ctx.into_diagnostics())
}

#[cfg(test)]
mod tests {
    const PETSTORE: &[u8] = include_bytes!("../../../corpus/specs/petstore-31.yaml");

    #[test]
    fn every_reference_is_accounted_for() {
        let counts = super::resolution(PETSTORE).unwrap();
        assert_eq!(
            counts.references,
            counts.resolved + counts.repaired + counts.dangling + counts.external
        );
        assert!(counts.references > 0);
    }
}
