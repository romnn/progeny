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

use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use proc_macro2::TokenStream;
use progeny::harness::Payload;
use quote::{format_ident, quote};

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

    /// How many documents to check at once.
    ///
    /// Defaults to four, or fewer when the cores or the free memory cannot carry four, at about
    /// 10 GiB per compile.
    /// Each compiles in its own cargo target directory, because cargo locks one for a whole build.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,
}

/// What one document's payloads did.
#[derive(Debug, Default)]
struct Outcome {
    checked: usize,
    failures: Vec<String>,
    vendor: Vec<String>,
    skipped: Skipped,
    /// Where `--generate-only` wrote the test.
    generated: Option<camino::Utf8PathBuf>,
}

/// The examples no test can check, by why, as [`progeny::harness::Payloads`] counts them.
///
/// Copied out of `Payloads`, whose payloads hold token streams, which cannot leave the worker
/// thread that collected them.
#[derive(Debug, Default)]
struct Skipped {
    opaque: usize,
    unnamed: usize,
    captures: usize,
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

    let budget = match args.serde {
        Some(crate::corpus::SerdeChoice::Derive) => crate::jobs::Budget::CompileDerive,
        _ => crate::jobs::Budget::Compile,
    };
    let jobs = crate::jobs::resolve(args.jobs, budget);
    println!("payloads: {} documents, {jobs} at a time", documents.len());
    // Each document compiles in its own worker's target directory, and reports in order.
    let outcomes = crate::jobs::map_ordered(&documents, jobs, |(spec, bytes), slot| {
        check(&spec.name, spec, bytes, args, slot)
    })?;
    let mut failures = 0usize;
    let mut totals = (0usize, 0usize, 0usize);
    for ((spec, _), outcome) in documents.iter().zip(outcomes) {
        let name = &spec.name;
        let outcome = outcome?;
        if let Some(file) = &outcome.generated {
            println!("  {name} → {file}");
        }
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
    slot: usize,
) -> eyre::Result<Outcome> {
    let mut config = crate::corpus::config_for(spec);
    // Types only: the payload question is about the shared type layer, and compiling an HTTP stack
    // per document to ask it would be minutes of nothing.
    config.emit.client = false;
    config.emit.server = false;
    if let Some(choice) = args.serde {
        config.serde_impl = choice.into();
    }
    round_trip(name, bytes, &config, args.generate_only, slot)
}

/// Generate one document's crate and its payload test, and run the test unless `generate_only`.
fn round_trip(
    name: &str,
    bytes: &[u8],
    config: &progeny::Config,
    generate_only: bool,
    slot: usize,
) -> eyre::Result<Outcome> {
    let collected = progeny::harness::payloads(bytes, config)
        .wrap_err_with(|| format!("collecting payloads from {name}"))?;
    let mut outcome = Outcome {
        skipped: Skipped {
            opaque: collected.opaque,
            unnamed: collected.unnamed,
            captures: collected.captures,
        },
        ..Outcome::default()
    };
    if collected.payloads.is_empty() {
        return Ok(outcome);
    }
    outcome.checked = collected.payloads.len();

    let output = progeny::generate(bytes, config)
        .wrap_err_with(|| format!("generating {name} for the payload gate"))?;
    let directory = crate::generated::write(&format!("payloads-{name}"), &output)?;
    let source = crate::generated::rust_source(
        test_source(&config.package.name, &collected.payloads)?,
        "tests/payloads.rs",
        name,
    )?;
    crate::generated::write_test(&directory, "payloads.rs", &source)?;
    let file = directory.join("tests/payloads.rs");

    if generate_only {
        outcome.generated = Some(file);
        return Ok(outcome);
    }

    let run = crate::generated::cargo_in(&directory, slot)
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

/// The comparison the test makes, which xtask compiles for its own tests.
const COMPARISON: &str = include_str!("comparison.rs");

/// The loader's decimal normalization, which the comparison compares numbers with.
const DECIMAL: &str = include_str!("../../crates/progeny/src/load/decimal.rs");

/// The test that goes into the generated crate.
///
/// One `check::<T>` call per payload, because the type has to be named at compile time. Failures
/// are printed rather than asserted one by one, so a document reports every payload that did not
/// round-trip instead of only the first.
fn test_source(package: &str, payloads: &[Payload]) -> eyre::Result<TokenStream> {
    let krate = format_ident!("{}", crate::corpus::lib_name(package));
    let checks = payloads.iter().map(|payload| {
        let ty = type_path(&payload.type_name, payload.read);
        let location = payload.location.as_str();
        let original = payload.original.to_string();
        let expected = payload.expected.to_string();
        let vendor_defect = payload.vendor_defect;
        let formatted = payload.formatted.iter().map(|leaf| {
            let at = leaf.at.as_str();
            let leaf_ty = &leaf.ty;
            let equivalent = if leaf.clock {
                quote! { comparison::same_clock::<#leaf_ty> }
            } else {
                quote! { comparison::same::<#leaf_ty> }
            };
            quote! { comparison::Formatted { at: #at, equivalent: #equivalent } }
        });
        quote! {
            check::<#ty>(
                &mut report,
                #location,
                #original,
                #expected,
                #vendor_defect,
                &[#(#formatted),*],
            );
        }
    });
    let runner = runner();
    let comparison = shipped(COMPARISON, "comparison.rs")?;
    let decimal = shipped(DECIMAL, "decimal.rs")?;
    Ok(quote! {
        //! Generated by `cargo xtask payloads`. Every example payload the document carries,
        //! deserialized into the type progeny generated for it and serialized back.

        use color_eyre::eyre;
        use #krate::types;

        // The name a formatted string's type is spelled through.
        extern crate #krate as generated;

        mod comparison {
            #comparison
        }

        mod decimal {
            #decimal
        }

        #runner

        #[test_util::test]
        fn every_example_payload_round_trips() {
            let mut report = Report::default();
            #(#checks)*
            report.finish();
        }
    })
}

/// A source file shipped into the test, as tokens.
fn shipped(source: &str, file: &str) -> eyre::Result<TokenStream> {
    source
        .parse()
        .map_err(|error| eyre::eyre!("reading {file} for the payload test: {error}"))
}

/// A payload's generated type, named from a file that imports the generated `types` module.
pub(crate) fn type_path(type_name: &str, read: bool) -> TokenStream {
    let name = format_ident!("{type_name}");
    if read {
        quote! { types::read::#name }
    } else {
        quote! { types::#name }
    }
}

/// The check of one payload, shipped into the generated test.
///
/// The payload goes to the type as the text the document wrote, and the type's output is compared
/// as the text it wrote.
/// A `serde_json::Value` in between would hold a number past what `f64` or a 64-bit integer holds
/// as a different one, and the comparison would be about that number instead.
fn runner() -> TokenStream {
    quote! {
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
            formatted: &[comparison::Formatted],
        ) where
            T: serde::de::DeserializeOwned + serde::Serialize,
        {
            let outcome = (|| -> std::result::Result<(), String> {
                let typed: T = serde_json::from_str(original)
                    .map_err(|error| format!("does not deserialize: {error}"))?;
                let again = serde_json::to_string(&typed)
                    .map_err(|error| format!("does not serialize: {error}"))?;
                match comparison::difference(expected, &again, formatted) {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::comparison::{Formatted, difference, same, same_clock};

    /// Two spellings of one number are one value, and two numbers one `f64` cannot tell apart are
    /// still two.
    #[test]
    fn numbers_compare_by_their_exact_value() {
        // Spellings of one value
        for (expected, actual) in [
            ("1", "1.0"),
            ("1e2", "100"),
            ("-0", "0.0"),
            ("1.50", "15E-1"),
        ] {
            assert_eq!(
                difference(expected, actual, &[]),
                None,
                "{expected} and {actual}"
            );
        }

        // Values one `f64` holds as one: an integer past 2^53, and two decimals
        for (expected, actual) in [
            ("9007199254740993", "9007199254740992.0"),
            ("0.1", "0.10000000000000001"),
        ] {
            assert_eq!(
                difference(expected, actual, &[]),
                Some(format!("`` was {expected} and came back {actual}")),
            );
        }
    }

    /// A number keeps its exact value inside objects and arrays too, and a change there is
    /// reported at its JSON Pointer.
    #[test]
    fn a_nested_number_is_compared_exactly() {
        assert_eq!(
            difference(r#"{"a/b": [1, 2.50]}"#, r#"{"a/b":[1.0,2.5]}"#, &[]),
            None
        );
        assert_eq!(
            difference(
                r#"{"a/b": [1, 9007199254740993]}"#,
                r#"{"a/b":[1,9007199254740992.0]}"#,
                &[],
            ),
            Some("`/a~1b/1` was 9007199254740993 and came back 9007199254740992.0".to_owned()),
        );
    }

    /// A lost member, an invented one, a shorter list and a changed string are each reported.
    #[test]
    fn a_change_in_shape_or_text_is_reported() {
        for (expected, actual, reason) in [
            (r#"{"a":1}"#, "{}", "the round trip dropped `/a`"),
            ("{}", r#"{"a":1}"#, "the round trip invented `/a`"),
            ("[1,2]", "[1]", "`` had 2 elements and came back with 1"),
            (
                r#"{"a":"x"}"#,
                r#"{"a":"X"}"#,
                r#"`/a` was "x" and came back "X""#,
            ),
            (
                r#"{"a":"1"}"#,
                r#"{"a":1}"#,
                r#"`/a` was "1" and came back 1"#,
            ),
        ] {
            assert_eq!(difference(expected, actual, &[]).as_deref(), Some(reason));
        }
        // A string is its text, whatever its escapes.
        assert_eq!(difference(r#""a\/b""#, r#""a/b""#, &[]), None);
    }

    /// A formatted string compares as the type it is held in reads it, and only where it is held
    /// in that type.
    #[test]
    fn a_formatted_string_compares_as_its_type_reads_it() {
        let ipv6 = [Formatted {
            at: "/address",
            equivalent: same::<std::net::Ipv6Addr>,
        }];
        let expanded = r#"{"address":"2001:0DB8:0000:0000:0000:0000:0000:0001"}"#;

        // The type's own spelling of the same address
        assert_eq!(
            difference(expanded, r#"{"address":"2001:db8::1"}"#, &ipv6),
            None
        );
        // A different address
        assert!(difference(expanded, r#"{"address":"2001:db8::2"}"#, &ipv6).is_some());
        // The same respelling of a plain string
        assert!(difference(expanded, r#"{"address":"2001:db8::1"}"#, &[]).is_some());
        // Two spellings the type cannot read are not one value.
        assert!(difference(r#"{"address":"a"}"#, r#"{"address":"A"}"#, &ipv6).is_some());
    }

    /// A date-time or time whose text holds more than any clock type keeps is never one value
    /// with another spelling, because the type reads it as a value it does not spell.
    /// `String` stands in for a type that reads both spellings as one value.
    #[test]
    fn a_clock_reading_past_what_its_types_hold_is_never_equivalent() {
        for (text, beyond) in [
            ("2024-01-01T00:00:00Z", false),
            ("2024-01-01T01:00:00.123456789+01:00", false),
            ("12:00:00.1234567890Z", false),
            ("2024-01-01T00:00:00.1234567891Z", true),
            ("2016-12-31T23:59:60Z", true),
            ("23:59:60.5-08:00", true),
        ] {
            let literal = format!("{text:?}");
            assert_eq!(same_clock::<String>(&literal, &literal), !beyond, "{text}");
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use color_eyre::eyre;

    /// An integer past 2^53 survives an integer field and is rounded by a `number` field, whose
    /// generated type is `f64`, and the gate reports exactly that one payload.
    #[test_util::test]
    fn a_large_integer_survives_an_integer_field_and_not_a_number_field() {
        let document = indoc::indoc! {r#"
            openapi: "3.1.0"
            info:
              title: Precision
              version: "1.0"
            paths:
              /counts:
                post:
                  requestBody:
                    content:
                      application/json:
                        schema:
                          type: object
                          properties:
                            count:
                              type: integer
                              format: int64
                        example:
                          count: 9007199254740993
                  responses:
                    "204":
                      description: Counted
              /ratios:
                post:
                  requestBody:
                    content:
                      application/json:
                        schema:
                          type: object
                          properties:
                            ratio:
                              type: number
                        example:
                          ratio: 9007199254740993
                  responses:
                    "204":
                      description: Measured
        "#};
        let mut config = progeny::Config::default();
        config.package.name = "payload-precision".to_owned();
        config.emit.client = false;
        config.emit.server = false;

        let outcome = super::round_trip("precision", document.as_bytes(), &config, false, 0)?;

        assert_eq!(outcome.checked, 2);
        assert_eq!(
            outcome.failures,
            [
                "/paths/~1ratios/post/requestBody/content/application~1json/example: `/ratio` \
                 was 9007199254740993 and came back 9007199254740992.0"
            ]
        );
    }

    /// Formatted strings the generated types respell come back as the same values: an expanded
    /// upper-case IPv6 address in a member and as the string beside two tagged objects, an
    /// upper-case UUID, and a date-time at an offset.
    /// A date-time whose digits past the nanosecond the type drops still fails.
    #[test_util::test]
    fn a_respelled_formatted_string_is_the_same_value() {
        let document = indoc::indoc! {r##"
            openapi: "3.1.0"
            info:
              title: Formats
              version: "1.0"
            paths:
              /hosts:
                post:
                  requestBody:
                    content:
                      application/json:
                        schema:
                          type: object
                          properties:
                            address:
                              type: string
                              format: ipv6
                            id:
                              type: string
                              format: uuid
                            seen:
                              type: string
                              format: date-time
                        examples:
                          respelled:
                            value:
                              address: "2001:0DB8:0000:0000:0000:0000:0000:0001"
                              id: "6FA459EA-EE8A-3CA4-894E-DB77E160355E"
                              seen: "2024-01-01T01:00:00.5+01:00"
                          truncated:
                            value:
                              seen: "2024-01-01T00:00:00.1234567891Z"
                  responses:
                    "204":
                      description: Stored
              /detections:
                post:
                  requestBody:
                    content:
                      application/json:
                        schema:
                          $ref: "#/components/schemas/Detection"
                        example: "2001:0DB8:0000:0000:0000:0000:0000:0001"
                  responses:
                    "204":
                      description: Stored
            components:
              schemas:
                Detection:
                  oneOf:
                    - type: string
                      format: ipv6
                    - $ref: "#/components/schemas/Google"
                    - $ref: "#/components/schemas/Vapi"
                Google:
                  type: object
                  required: [provider]
                  properties:
                    provider:
                      type: string
                      enum: [google]
                Vapi:
                  type: object
                  required: [provider]
                  properties:
                    provider:
                      type: string
                      enum: [vapi]
        "##};
        let mut config = progeny::Config::default();
        config.package.name = "payload-formats".to_owned();
        config.emit.client = false;
        config.emit.server = false;
        config.formats.date_time = progeny::DateTimeCrate::Chrono;
        config.formats.uuid = progeny::UuidCrate::Uuid;

        let outcome = super::round_trip("formats", document.as_bytes(), &config, false, 0)?;

        assert_eq!(outcome.checked, 3);
        assert_eq!(
            outcome.failures,
            [
                "/paths/~1hosts/post/requestBody/content/application~1json/examples/truncated: \
                 `/seen` was \"2024-01-01T00:00:00.1234567891Z\" and came back \
                 \"2024-01-01T00:00:00.123456789Z\""
            ]
        );
    }
}
