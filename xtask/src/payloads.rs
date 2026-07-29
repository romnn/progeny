//! The payload gate: run serde against the documents' own example payloads.
//!
//! Every other gate in this project asks a question about *source*. This one asks a question about
//! *data*, and it is the only one that can: the five defects stage 4's review found all generated
//! code that compiled, snapshotted and round-tripped its document, and were wrong only about
//! payloads.
//!
//! It works by generating a crate and then generating a test *into* that crate, because the check
//! has to name the generated types statically — there is no way to deserialize into a type chosen
//! at run time. The library says which type each example belongs to and what a faithful round trip
//! keeps; this turns that into Rust and runs it.
//!
//! Two rules it is built with, both learned rather than assumed:
//!
//! * **Compare against the original payload, never a second round of the type's own output.** A
//!   member the type drops uniformly survives an idempotence check forever.
//! * **Carry a vendor verdict from the start.** 19 corpus documents write examples that contradict
//!   their own schemas. A harness with no verdict for them reports 19 failures nobody can fix.

use std::fmt::Write as _;

use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use progeny::harness::{Payload, Payloads};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Corpus documents to check. Defaults to the quick tier.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Check every document in the manifest rather than the quick tier.
    #[arg(long)]
    all: bool,

    /// Write the generated test crates and stop, without compiling or running anything.
    #[arg(long)]
    generate_only: bool,

    /// Which serde strategy to generate with, rather than the configuration default.
    ///
    /// This gate is the only one that runs serde against real payloads, so it is the only one that
    /// can tell the two strategies apart on the wire at corpus scale. The differential harness
    /// compares them on one fixture; this compares each against 643 payloads the vendors wrote.
    #[arg(long, value_name = "STRATEGY")]
    serde: Option<crate::corpus::SerdeChoice>,
}

/// What one document's payloads did.
#[derive(Debug, Default)]
struct Outcome {
    checked: usize,
    failures: Vec<String>,
    vendor: Vec<String>,
    skipped: Payloads,
}

pub fn run(args: &Args) -> eyre::Result<()> {
    crate::generated::require_cargo()?;
    let wanted = if !args.specs.is_empty() {
        args.specs.clone()
    } else if args.all {
        crate::corpus::load_manifest()?
            .iter()
            .map(|spec| spec.name.clone())
            .collect()
    } else {
        crate::corpus::quick_tier()?
    };
    // A missing document is an error, not a skip: this gate once printed `skipped` per missing
    // file and exited green having checked zero payloads — coverage it did not have, reported as
    // coverage it did.
    let documents = crate::corpus::selected(&wanted)?;

    println!("payloads: {} documents", documents.len());
    let mut failures = 0usize;
    let mut totals = (0usize, 0usize, 0usize);
    for (spec, bytes) in &documents {
        let name = &spec.name;
        let outcome = check(name, spec, bytes, args)?;
        totals.0 += outcome.checked;
        totals.1 += outcome.vendor.len();
        totals.2 += outcome.skipped.opaque + outcome.skipped.unnamed + outcome.skipped.captures;
        if outcome.failures.is_empty() {
            println!(
                "  ok        {name:<24} {} payloads round-tripped, {} vendor defects, {} not \
                 checkable",
                outcome.checked,
                outcome.vendor.len(),
                outcome.skipped.opaque + outcome.skipped.unnamed + outcome.skipped.captures,
            );
        } else {
            failures += 1;
            println!(
                "  FAILED    {name:<24} {} of {} payloads did not round-trip",
                outcome.failures.len(),
                outcome.checked
            );
            for failure in outcome.failures.iter().take(5) {
                println!("              {failure}");
            }
        }
        // Never silent about what was left out: a gate that skips quietly reads as coverage it
        // does not have.
        if outcome.skipped.opaque + outcome.skipped.unnamed + outcome.skipped.captures > 0 {
            println!(
                "              not checkable: {} arbitrary JSON, {} unnamed types, {} capture \
                 undeclared members",
                outcome.skipped.opaque, outcome.skipped.unnamed, outcome.skipped.captures
            );
        }
    }

    println!();
    println!(
        "payloads: {} checked, {} vendor defects tolerated, {} positions not checkable",
        totals.0, totals.1, totals.2
    );
    if failures > 0 {
        bail!("{failures} documents have payloads that do not round-trip");
    }
    Ok(())
}

fn check(
    name: &str,
    spec: &crate::corpus::Spec,
    bytes: &[u8],
    args: &Args,
) -> eyre::Result<Outcome> {
    let mut config = crate::corpus::config_for(spec);
    // Types only: the payload question is about the shared type layer, and compiling an HTTP stack
    // per document to ask it would be minutes of nothing.
    config.emit.client = false;
    config.emit.server = false;
    if let Some(choice) = args.serde {
        config.serde_impl = choice.into();
    }

    let collected = progeny::harness::payloads(bytes, &config)
        .wrap_err_with(|| format!("collecting payloads from {name}"))?;
    let mut outcome = Outcome {
        skipped: Payloads {
            payloads: Vec::new(),
            ..collected
        },
        ..Outcome::default()
    };
    if collected.payloads.is_empty() {
        return Ok(outcome);
    }
    outcome.checked = collected.payloads.len();

    let output = progeny::generate(bytes, &config)
        .wrap_err_with(|| format!("generating {name} for the payload gate"))?;
    let directory = crate::generated::write(&format!("payloads-{name}"), &output)?;
    let source = test_source(&config.package.name, &collected.payloads);
    crate::generated::write_test(&directory, "payloads.rs", &source)?;
    let file = directory.join("tests/payloads.rs");

    if args.generate_only {
        println!("  {name} → {file}");
        return Ok(outcome);
    }

    let run = crate::generated::cargo(&directory)
        // `--nocapture`, because the report travels on stdout and libtest swallows the stdout of a
        // test that passes — without it a document with vendor defects and no real failures would
        // report zero of both, which is the reading that looks like success.
        .args(["test", "--quiet", "--test", "payloads", "--", "--nocapture"])
        .output()
        .wrap_err_with(|| format!("running the payload test for {name}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    if !run.status.success() && !text.contains("PAYLOAD ") {
        bail!(
            "{}",
            indoc::formatdoc! {"
                the payload test for {name} did not run:
                {text}"
            }
        );
    }
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("PAYLOAD FAIL ") {
            outcome.failures.push(rest.to_owned());
        } else if let Some(rest) = line.trim().strip_prefix("PAYLOAD VENDOR ") {
            outcome.vendor.push(rest.to_owned());
        }
    }
    Ok(outcome)
}

/// The test that goes into the generated crate.
///
/// One `check::<T>` call per payload, because the type has to be named at compile time. Failures
/// are printed rather than asserted one by one, so a document reports every payload that did not
/// round-trip instead of only the first.
fn test_source(package: &str, payloads: &[Payload]) -> String {
    let krate = crate::corpus::lib_name(package);
    let mut out = String::new();
    out.push_str(indoc::indoc! {"
        //! Generated by `cargo xtask payloads`. Every example payload the document carries,
        //! deserialized into the type progeny generated for it and serialized back.

    "});
    let _ = writeln!(
        out,
        "{}",
        indoc::formatdoc! {"
            use color_eyre::eyre;
            use {krate}::types;
        "}
    );
    out.push_str(RUNNER);
    out.push_str(indoc::indoc! {"

        #[test_util::test]
        fn every_example_payload_round_trips() {
    "});
    out.push_str("    let mut report = Report::default();\n");
    for payload in payloads {
        let _ = writeln!(
            out,
            "    check::<types::{}>(&mut report, {:?}, {:?}, {:?}, {})?;",
            payload.type_name,
            payload.location,
            payload.original.to_string(),
            payload.expected.to_string(),
            payload.vendor_defect,
        );
    }
    out.push_str(indoc::indoc! {"
            report.finish();
        }
    "});
    out
}

/// The comparison, shipped into the generated test.
///
/// Numbers are compared as numbers: a document writing `1` for a field progeny typed `f64` gets
/// `1.0` back, and `serde_json::Value` equality would call that a loss. It is not one — the value
/// survived, and JSON has one number type.
const RUNNER: &str = indoc::indoc! {r#"
#[derive(Default)]
struct Report {
    failures: usize,
}

impl Report {
    fn finish(&self) {
        assert_eq!(self.failures, 0, "{} payloads did not round-trip", self.failures);
    }
}

fn check<T>(
    report: &mut Report,
    location: &str,
    original: &str,
    expected: &str,
    vendor_defect: bool,
) -> eyre::Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let original: serde_json::Value = serde_json::from_str(original)?;
    let expected: serde_json::Value = serde_json::from_str(expected)?;
    let outcome = (|| -> std::result::Result<(), String> {
        let typed: T = serde_json::from_value(original.clone())
            .map_err(|error| format!("does not deserialize: {error}"))?;
        let again = serde_json::to_value(&typed)
            .map_err(|error| format!("does not serialize: {error}"))?;
        match difference(&expected, &again, "") {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    })();
    if let Err(reason) = outcome {
        if vendor_defect {
            // The document contradicts itself here, which progeny already reported as
            // `invalid-example`. Counted, never failed: it is a finding about the vendor.
            println!("PAYLOAD VENDOR {location}: {reason}");
        } else {
            report.failures += 1;
            println!("PAYLOAD FAIL {location}: {reason}");
        }
    }
    Ok(())
}

/// Where the round trip lost or changed something, if it did.
fn difference(expected: &serde_json::Value, actual: &serde_json::Value, at: &str) -> Option<String> {
    use serde_json::Value;
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                let at = format!("{at}/{key}");
                match actual.get(key) {
                    Some(other) => {
                        if let Some(reason) = difference(value, other, &at) {
                            return Some(reason);
                        }
                    }
                    None => return Some(format!("the round trip dropped `{at}`")),
                }
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    return Some(format!("the round trip invented `{at}/{key}`"));
                }
            }
            None
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                return Some(format!(
                    "`{at}` had {} elements and came back with {}",
                    expected.len(),
                    actual.len()
                ));
            }
            for (index, (value, other)) in expected.iter().zip(actual).enumerate() {
                if let Some(reason) = difference(value, other, &format!("{at}/{index}")) {
                    return Some(reason);
                }
            }
            None
        }
        (Value::Number(expected), Value::Number(actual)) => {
            // JSON has one number type; `1` and `1.0` are the same value written twice.
            match (expected.as_f64(), actual.as_f64()) {
                (Some(left), Some(right)) if left == right => None,
                _ if expected == actual => None,
                _ => Some(format!("`{at}` was {expected} and came back {actual}")),
            }
        }
        (expected, actual) if expected == actual => None,
        (expected, actual) => Some(format!("`{at}` was {expected} and came back {actual}")),
    }
}
"#};
