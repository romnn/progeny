//! The differential serde harness, and the body count that justifies it.
//!
//! One document generated twice — once with the derive, once with the hand-written path — into one
//! test crate that asserts the two are equivalent on the wire. That is what makes the strategy an
//! implementation detail rather than a compatibility decision: if the two ever disagree, the
//! difference is a test failure here instead of a support ticket later.
//!
//! The body count is the other half. The claim behind the hand-written path is "two function bodies
//! per type instead of about nine", and it is measured rather than remembered: generate N copies of
//! one shape, expand the crate, and difference the count at N=1 against N=11. Expansion is
//! deterministic, so the number is valid on a loaded machine — no idle-machine discipline needed.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, ContextCompat, WrapErr, bail};
use progeny::{Config, Package, Packaging, SerdeImpl};

use crate::paths;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Also count function bodies per type, by differencing expanded output. Needs nightly.
    #[arg(long)]
    bodies: bool,

    /// Keep the assembled crate instead of leaving only what the run needed.
    #[arg(long)]
    keep: bool,
}

/// The function bodies every generated type carries whichever serde strategy it takes.
///
/// The always-emitted derives cost one body each, so they are the same on both sides of the
/// comparison and subtracting them is what makes the remaining number about serde. Asked of the
/// library rather than restated: a hand-kept `2.0` here would silently under-count the day a
/// third always-emitted derive lands, in the ratio the hand-written path is justified by.
#[expect(
    clippy::cast_precision_loss,
    reason = "a derive count of two or three fits in f64 exactly"
)]
fn baseline() -> f64 {
    progeny::harness::base_derive_count() as f64
}

fn fixture_root() -> Utf8PathBuf {
    paths::corpus_root().join("differential")
}

fn scratch() -> Utf8PathBuf {
    paths::workspace_root().join("target/differential")
}

pub fn run(args: &Args) -> eyre::Result<()> {
    let document = std::fs::read(fixture_root().join("spike.yaml"))
        .wrap_err_with(|| format!("reading {}/spike.yaml", fixture_root()))?;
    let assertions = std::fs::read_to_string(fixture_root().join("assertions.rs"))
        .wrap_err_with(|| format!("reading {}/assertions.rs", fixture_root()))?;

    let directory = scratch();
    assemble(&directory, &document, &assertions)?;
    println!("differential: assembled {directory}");

    // The shared dependency cache every other gate uses: this gate once kept a private target
    // directory and recompiled `serde` and `serde_json` from scratch on each run, invisibly,
    // because the decision sat in a module nobody reads beside `generated`.
    let status = crate::generated::cargo(&directory)
        .args(["test", "--quiet"])
        .status()
        .wrap_err_with(|| format!("running cargo test in {directory}"))?;
    if !status.success() {
        bail!("the two renderings disagree; see the failures above");
    }
    println!("differential: the derive and the hand-written path agree on every case");

    if args.bodies {
        count_bodies()?;
    }
    if !args.keep {
        // The `target` directory inside is what makes this expensive to keep around; the source is
        // small and worth leaving for inspection.
        let _ = std::fs::remove_dir_all(directory.join("target"));
    }
    Ok(())
}

/// Write the test crate: both renderings of one document, plus the assertions.
fn assemble(directory: &Utf8Path, document: &[u8], assertions: &str) -> eyre::Result<()> {
    if directory.exists() {
        // Only the sources; keeping `target` is what makes a second run quick.
        for stale in ["src", "tests", "Cargo.toml"] {
            let path = directory.join(stale);
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
    std::fs::create_dir_all(directory.join("src"))
        .wrap_err_with(|| format!("creating {directory}/src"))?;
    std::fs::create_dir_all(directory.join("tests"))
        .wrap_err_with(|| format!("creating {directory}/tests"))?;

    for (module, strategy) in [
        ("derived", SerdeImpl::DeriveAlways),
        ("hand", SerdeImpl::HandWrittenWhereEligible),
    ] {
        let rendered = render(document, strategy)?;
        std::fs::write(directory.join(format!("src/{module}.rs")), rendered)
            .wrap_err_with(|| format!("writing {directory}/src/{module}.rs"))?;
    }

    std::fs::write(
        directory.join("src/lib.rs"),
        indoc::indoc! {"
            //! Both renderings of one document, for the differential harness.
            pub mod derived;
            pub mod hand;
        "},
    )?;
    std::fs::write(directory.join("tests/differential.rs"), assertions)?;
    // The edition and dependency lines restate what `render/manifest.rs` writes for a shipped
    // crate, and they have to keep restating it: this gate certifies "the derive and the
    // hand-written path agree", and a crate compiled under a different edition than any shipped
    // configuration would quietly make that claim about something nobody ships.
    std::fs::write(
        directory.join("Cargo.toml"),
        indoc::indoc! {r#"
            # Assembled by `cargo xtask differential`.
            [package]
            name = "differential"
            version = "0.0.0"
            edition = "2021"
            publish = false

            [dependencies]
            serde = { version = "1", features = ["derive"] }
            serde_json = "1"

            [dev-dependencies]
            color-eyre = "0.6"
            test-util = { path = "../../crates/test-util" }

            [workspace]
        "#},
    )?;
    Ok(())
}

/// Generate one document in module mode, which is the packaging that fits into another crate.
fn render(document: &[u8], strategy: SerdeImpl) -> eyre::Result<String> {
    let config = Config {
        serde_impl: strategy,
        preserve_optional_nullable: true,
        packaging: Packaging::Module,
        // The two serde renderings are a question about the shared type layer; a client would be
        // the same source in both halves and an HTTP stack compiled twice to prove it.
        emit: progeny::Emit {
            client: false,
            server: false,
            ..progeny::Emit::default()
        },
        package: Package {
            name: "spike".to_owned(),
            version: "0.0.0".to_owned(),
        },
        ..Config::default()
    };
    let output = progeny::generate(document, &config).wrap_err("generating the spike")?;
    output
        .files
        .get(Utf8Path::new("progeny.rs"))
        .cloned()
        .wrap_err("module-mode generation produced no module")
}

/// Difference the expanded output of a crate with 1 and with 11 copies of one shape.
fn count_bodies() -> eyre::Result<()> {
    let mut counts = Vec::new();
    for strategy in [SerdeImpl::DeriveAlways, SerdeImpl::HandWrittenWhereEligible] {
        let mut measured = Vec::new();
        for copies in [1_usize, 11] {
            let document = synthesize(copies);
            let directory = scratch().join(format!("bodies-{copies}"));
            assemble_single(&directory, document.as_bytes(), strategy)?;
            measured.push(expand_and_count(&directory)?);
        }
        let (one, eleven) = (
            measured.first().copied().unwrap_or_default(),
            measured.get(1).copied().unwrap_or_default(),
        );
        let per_type = difference_per_type(eleven.total, one.total);
        let generic_per_type = difference_per_type(eleven.generic, one.generic);
        let serde_only = per_type - baseline();
        let name = match strategy {
            SerdeImpl::DeriveAlways => "derive",
            SerdeImpl::HandWrittenWhereEligible => "hand-written",
        };
        println!(
            "bodies: {name:<12} {:>4} at N=1, {:>4} at N=11 → {per_type:.1} per type, of which \
             {serde_only:.1} are serde's; {generic_per_type:.1} generic bodies per type",
            one.total, eleven.total,
        );
        counts.push((name, serde_only));
    }
    if let (Some((_, derive)), Some((_, hand))) = (counts.first(), counts.get(1))
        && *hand > 0.0
    {
        println!(
            "bodies: the hand-written serde path is {:.1}× fewer function bodies per type \
             ({derive:.0} against {hand:.0})",
            derive / hand
        );
    }
    Ok(())
}

fn difference_per_type(eleven: usize, one: usize) -> f64 {
    f64::from(u32::try_from(eleven.saturating_sub(one)).unwrap_or(u32::MAX)) / 10.0
}

/// A document with `copies` differently-named copies of the spike struct.
///
/// Named components, so deduplication leaves them alone: names are API, and two components that
/// look alike stay two types.
fn synthesize(copies: usize) -> String {
    let mut out = String::from(indoc::indoc! {r#"
        openapi: 3.1.0
        info:
          title: Bodies
          version: "1.0"
        paths: {}
        components:
          schemas:
    "#});
    for index in 0..copies {
        let _ = write!(
            out,
            "{}",
            indoc::formatdoc! {"
                {indent}Spike{index}:
                {indent}  type: object
                {indent}  required: [required]
                {indent}  properties:
                {indent}    required:
                {indent}      type: string
                {indent}    optional:
                {indent}      type: integer
                {indent}    wireName:
                {indent}      type: boolean
                ",
                indent = "    "
            }
        );
    }
    out
}

fn assemble_single(directory: &Utf8Path, document: &[u8], strategy: SerdeImpl) -> eyre::Result<()> {
    std::fs::create_dir_all(directory.join("src"))
        .wrap_err_with(|| format!("creating {directory}/src"))?;
    std::fs::write(directory.join("src/lib.rs"), render(document, strategy)?)?;
    std::fs::write(
        directory.join("Cargo.toml"),
        indoc::indoc! {r#"
            [package]
            name = "bodies"
            version = "0.0.0"
            edition = "2021"
            publish = false

            [dependencies]
            serde = { version = "1", features = ["derive"] }
            serde_json = "1"

            [workspace]
        "#},
    )?;
    Ok(())
}

/// Expand a crate and count the function bodies in it.
fn expand_and_count(directory: &Utf8Path) -> eyre::Result<BodyCounts> {
    let output = crate::generated::cargo(directory)
        // Its own target directory: expansion is a nightly build of the same crate the gate above
        // just checked on stable, and sharing one would have each invalidate the other's artefacts.
        .env("CARGO_TARGET_DIR", scratch().join("bodies-target"))
        .args(["+nightly", "rustc", "--quiet", "--", "-Zunpretty=expanded"])
        .output()
        .wrap_err_with(|| format!("expanding {directory}"))?;
    if !output.status.success() {
        bail!(
            "{}",
            indoc::formatdoc! {"
                expanding {directory} failed; nightly is needed for `-Zunpretty=expanded`:
                {}",
                String::from_utf8_lossy(&output.stderr)
            }
        );
    }
    let expanded = String::from_utf8_lossy(&output.stdout);
    // A body is a `fn` with a block. Counting the keyword is enough here because the difference,
    // not the absolute number, is the measurement — and both sides are counted the same way.
    Ok(body_counts(&expanded))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BodyCounts {
    total: usize,
    generic: usize,
}

fn body_counts(expanded: &str) -> BodyCounts {
    let mut counts = BodyCounts::default();
    for line in expanded.lines() {
        let trimmed = line.trim_start();
        let signature = ["fn ", "pub fn ", "const fn ", "pub const fn "]
            .into_iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix));
        let Some(signature) = signature else {
            continue;
        };
        counts.total += 1;
        if signature
            .find('(')
            .is_some_and(|arguments| signature[..arguments].contains('<'))
        {
            counts.generic += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    #[test]
    fn expanded_body_counts_distinguish_generic_functions() {
        let expanded = indoc::indoc! {"
            fn plain() {}
            pub fn generic<T>(value: T) {}
            const fn constant() {}
            struct NotAFunction;
        "};
        assert_eq!(
            super::body_counts(expanded),
            super::BodyCounts {
                total: 3,
                generic: 1,
            }
        );
    }

    #[test]
    fn assemble_experiment_record_is_valid_toml() {
        let record = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../corpus/assemble.toml"
        ));
        assert!(toml::from_str::<toml::Value>(record).is_ok());
    }
}
