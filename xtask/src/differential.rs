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
use std::process::Command;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
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
/// `Clone` and `Debug` are the derives progeny always emits, one body each, so they are the same on
/// both sides of the comparison and subtracting them is what makes the remaining number about serde.
const BASELINE: f64 = 2.0;

fn fixture_root() -> Utf8PathBuf {
    paths::corpus_root().join("differential")
}

fn scratch() -> Utf8PathBuf {
    paths::workspace_root().join("target/differential")
}

pub fn run(args: &Args) -> Result<()> {
    let document = std::fs::read(fixture_root().join("spike.yaml"))
        .with_context(|| format!("reading {}/spike.yaml", fixture_root()))?;
    let assertions = std::fs::read_to_string(fixture_root().join("assertions.rs"))
        .with_context(|| format!("reading {}/assertions.rs", fixture_root()))?;

    let directory = scratch();
    assemble(&directory, &document, &assertions)?;
    println!("differential: assembled {directory}");

    let status = Command::new("cargo")
        .current_dir(&directory)
        .env("CARGO_TARGET_DIR", directory.join("target"))
        .env_remove("RUSTFLAGS")
        .args(["test", "--quiet"])
        .status()
        .with_context(|| format!("running cargo test in {directory}"))?;
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
fn assemble(directory: &Utf8Path, document: &[u8], assertions: &str) -> Result<()> {
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
        .with_context(|| format!("creating {directory}/src"))?;
    std::fs::create_dir_all(directory.join("tests"))
        .with_context(|| format!("creating {directory}/tests"))?;

    for (module, strategy) in [
        ("derived", SerdeImpl::DeriveAlways),
        ("hand", SerdeImpl::HandWrittenWhereEligible),
    ] {
        let rendered = render(document, strategy)?;
        std::fs::write(directory.join(format!("src/{module}.rs")), rendered)
            .with_context(|| format!("writing {directory}/src/{module}.rs"))?;
    }

    std::fs::write(
        directory.join("src/lib.rs"),
        "//! Both renderings of one document, for the differential harness.\n\
         pub mod derived;\n\
         pub mod hand;\n",
    )?;
    std::fs::write(directory.join("tests/differential.rs"), assertions)?;
    std::fs::write(
        directory.join("Cargo.toml"),
        "# Assembled by `cargo xtask differential`.\n\
         [package]\n\
         name = \"differential\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\n\
         [dependencies]\n\
         serde = { version = \"1\", features = [\"derive\"] }\n\
         serde_json = \"1\"\n\n\
         [workspace]\n",
    )?;
    Ok(())
}

/// Generate one document in module mode, which is the packaging that fits into another crate.
fn render(document: &[u8], strategy: SerdeImpl) -> Result<String> {
    let config = Config {
        serde_impl: strategy,
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
    let output = progeny::generate(document, &config).context("generating the spike")?;
    output
        .files
        .get(Utf8Path::new("progeny.rs"))
        .cloned()
        .context("module-mode generation produced no module")
}

/// Difference the expanded output of a crate with 1 and with 11 copies of one shape.
fn count_bodies() -> Result<()> {
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
            measured.first().copied().unwrap_or(0),
            measured.get(1).copied().unwrap_or(0),
        );
        let per_type =
            f64::from(u32::try_from(eleven.saturating_sub(one)).unwrap_or(u32::MAX)) / 10.0;
        let serde_only = per_type - BASELINE;
        let name = match strategy {
            SerdeImpl::DeriveAlways => "derive",
            SerdeImpl::HandWrittenWhereEligible => "hand-written",
        };
        println!(
            "bodies: {name:<12} {one:>4} at N=1, {eleven:>4} at N=11 → {per_type:.1} per type, of \
             which {serde_only:.1} are serde's"
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

/// A document with `copies` differently-named copies of the spike struct.
///
/// Named components, so deduplication leaves them alone: names are API, and two components that
/// look alike stay two types.
fn synthesize(copies: usize) -> String {
    let mut out = String::from(
        "openapi: 3.1.0\ninfo:\n  title: Bodies\n  version: \"1.0\"\npaths: {}\ncomponents:\n  schemas:\n",
    );
    for index in 0..copies {
        let _ = write!(
            out,
            "    Spike{index}:\n      type: object\n      required: [required]\n      properties:\n        required:\n          type: string\n        optional:\n          type: integer\n        wireName:\n          type: boolean\n"
        );
    }
    out
}

fn assemble_single(directory: &Utf8Path, document: &[u8], strategy: SerdeImpl) -> Result<()> {
    std::fs::create_dir_all(directory.join("src"))
        .with_context(|| format!("creating {directory}/src"))?;
    std::fs::write(directory.join("src/lib.rs"), render(document, strategy)?)?;
    std::fs::write(
        directory.join("Cargo.toml"),
        "[package]\nname = \"bodies\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n\n\
         [workspace]\n",
    )?;
    Ok(())
}

/// Expand a crate and count the function bodies in it.
fn expand_and_count(directory: &Utf8Path) -> Result<usize> {
    let output = Command::new("cargo")
        .current_dir(directory)
        .env("CARGO_TARGET_DIR", scratch().join("bodies-target"))
        .env_remove("RUSTFLAGS")
        .args(["+nightly", "rustc", "--quiet", "--", "-Zunpretty=expanded"])
        .output()
        .with_context(|| format!("expanding {directory}"))?;
    if !output.status.success() {
        bail!(
            "expanding {directory} failed; nightly is needed for `-Zunpretty=expanded`:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let expanded = String::from_utf8_lossy(&output.stdout);
    // A body is a `fn` with a block. Counting the keyword is enough here because the difference,
    // not the absolute number, is the measurement — and both sides are counted the same way.
    Ok(expanded
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("const fn ")
                || trimmed.starts_with("pub const fn ")
        })
        .count())
}
