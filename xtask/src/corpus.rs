//! The conformance corpus: 78 published API descriptions, run through the model.
//!
//! Vendor documents are not committed — they total roughly 117 MB and their redistribution rights
//! vary by publisher — so `corpus/manifest.toml` carries the provenance and `--fetch` downloads
//! each one into a gitignored cache. The single exception is `petstore-31`, which is hand-written,
//! hermetic, and therefore the one document an offline run can always assert against.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
use progeny::harness::{self, Stats};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Download every document listed in the manifest that is not already cached.
    #[arg(long)]
    fetch: bool,

    /// Re-download even documents that are already cached.
    #[arg(long, requires = "fetch")]
    refresh: bool,

    /// Run only the quick tier from `corpus/tier.toml`.
    #[arg(long)]
    quick: bool,

    /// Run only these documents, by name. Repeatable.
    #[arg(long = "only", value_name = "NAME")]
    only: Vec<String>,

    /// Report the model-level counts the design questions turn on.
    #[arg(long)]
    stats: bool,

    /// Print every diagnostic each document produced, as JSON lines.
    #[arg(long)]
    show_diagnostics: bool,

    /// Write a per-document timing report here, as JSON.
    #[arg(long, value_name = "PATH")]
    timings: Option<Utf8PathBuf>,
}

/// One entry of `corpus/manifest.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    name: String,
    /// The OpenAPI version family the document declares, for cross-checking what was parsed.
    version: Option<String>,
    /// Upstream source. Absent for committed documents.
    url: Option<String>,
    /// Filename within the cache, or within `corpus/specs` when committed.
    file: Option<String>,
    /// Committed in-tree, hermetic, safe for pull-request CI without a network.
    #[serde(default)]
    local: bool,
    /// The document exercises the test-double path.
    #[serde(default)]
    mock: bool,
    /// Keep full JSON-schema doc blocks for this document.
    #[serde(default)]
    schema_docs: bool,
    /// Schema names whose own example payloads contradict their schema — a vendor defect, not
    /// progeny's. Excluded from payload round-trips once those exist.
    #[serde(default)]
    bad_examples: Vec<String>,
    /// What this document stresses, and why it is in the corpus.
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    spec: Vec<Spec>,
}

#[derive(Debug, Deserialize)]
struct Tier {
    quick: Vec<String>,
}

/// What happened to one document.
enum Outcome {
    Clean {
        schemas: usize,
        diagnostics: Vec<String>,
        yaml: bool,
        declared: String,
        /// Set when the document declares a different dialect than the manifest recorded, which
        /// means either the manifest went stale or the publisher moved.
        version_drift: Option<String>,
    },
    Differs(Vec<harness::Difference>),
    Nondeterministic,
    Rejected(String),
    /// The document could not be read at all. Not a finding about progeny.
    Unavailable(String),
}

pub fn run(args: &Args) -> Result<()> {
    let specs = load_manifest()?;
    lint_manifest(&specs)?;
    let selected = select(&specs, args)?;

    println!(
        "corpus: {} documents in the manifest, {} selected ({} committed, {} exercise the \
         test-double path, {} keep full schema docs, {} carry self-contradicting examples)",
        specs.len(),
        selected.len(),
        specs.iter().filter(|spec| spec.local).count(),
        specs.iter().filter(|spec| spec.mock).count(),
        specs.iter().filter(|spec| spec.schema_docs).count(),
        specs
            .iter()
            .filter(|spec| !spec.bad_examples.is_empty())
            .count(),
    );
    if selected.len() < specs.len() {
        let skipped: Vec<&str> = specs
            .iter()
            .filter(|spec| !selected.iter().any(|other| other.name == spec.name))
            .map(|spec| spec.name.as_str())
            .collect();
        // Never sample silently: a run that covered less has to say what it left out.
        println!("corpus: skipped {}", skipped.join(", "));
    }

    if args.fetch {
        fetch_all(&selected, args.refresh)?;
    }

    let mut outcomes: Vec<(String, Outcome, Duration)> = Vec::new();
    let mut totals = Stats::default();
    let mut counted = 0usize;

    for spec in &selected {
        let started = Instant::now();
        let outcome = check(spec, args, &mut totals, &mut counted);
        outcomes.push((spec.name.clone(), outcome, started.elapsed()));
    }

    report(&specs, &outcomes, args.show_diagnostics);
    if args.stats {
        report_stats(&totals, counted);
    }
    if let Some(path) = &args.timings {
        write_timings(path, &outcomes)?;
    }

    verdict(&outcomes)
}

fn load_manifest() -> Result<Vec<Spec>> {
    let path = paths::corpus_root().join("manifest.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let manifest: Manifest = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
    Ok(manifest.spec)
}

/// Check the manifest itself before trusting anything it says.
///
/// The two fields that carry hard-won knowledge — which documents ship examples that contradict
/// their own schemas, and what each document stresses — are only useful if they stay accurate, and
/// a duplicated or misplaced entry is the kind of rot nobody notices by reading.
fn lint_manifest(specs: &[Spec]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for spec in specs {
        if !seen.insert(spec.name.as_str()) {
            bail!("the manifest lists `{}` twice", spec.name);
        }
        if spec.local && spec.url.is_some() {
            bail!("`{}` is committed but also carries a url", spec.name);
        }
        if !spec.local && spec.url.is_none() {
            bail!("`{}` is neither committed nor fetchable", spec.name);
        }
        let mut names = spec.bad_examples.clone();
        names.sort();
        names.dedup();
        if names.len() != spec.bad_examples.len() {
            bail!("`{}` lists a schema twice under bad_examples", spec.name);
        }
    }
    Ok(())
}

fn select(specs: &[Spec], args: &Args) -> Result<Vec<Spec>> {
    if !args.only.is_empty() {
        let mut selected = Vec::new();
        for name in &args.only {
            let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
                bail!("no corpus document named `{name}`");
            };
            selected.push(spec.clone());
        }
        return Ok(selected);
    }
    if args.quick {
        let path = paths::corpus_root().join("tier.toml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
        let tier: Tier = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
        let mut selected = Vec::new();
        for name in &tier.quick {
            let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
                bail!("the quick tier names `{name}`, which is not in the manifest");
            };
            selected.push(spec.clone());
        }
        return Ok(selected);
    }
    Ok(specs.to_vec())
}

/// Where a document lives on disk.
///
/// A committed document lives in `corpus/specs`; everything else in the cache. The cached
/// filename is the manifest's when it has one, and otherwise the document's name with an
/// extension guessed from the URL — several corpus documents are served from extensionless URLs,
/// and one of those serves YAML from a name that ends up `.json`, which is why nothing downstream
/// may dispatch on the extension.
fn document_path(spec: &Spec) -> Utf8PathBuf {
    if spec.local {
        let file = spec.file.clone().unwrap_or_else(|| spec.name.clone());
        return paths::specs_root().join(file);
    }
    let file = spec.file.clone().unwrap_or_else(|| {
        let yaml = spec.url.as_deref().is_some_and(|url| {
            std::path::Path::new(url)
                .extension()
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml")
                })
        });
        format!("{}.{}", spec.name, if yaml { "yaml" } else { "json" })
    });
    paths::cache_root().join(file)
}

fn fetch_all(specs: &[Spec], refresh: bool) -> Result<()> {
    let mut fetched = 0usize;
    let mut failed = Vec::new();
    for spec in specs {
        if spec.local {
            continue;
        }
        let path = document_path(spec);
        if path.exists() && !refresh {
            continue;
        }
        let Some(url) = spec.url.as_deref() else {
            failed.push(format!("{}: no url in the manifest", spec.name));
            continue;
        };
        print!("fetching {} … ", spec.name);
        match download(url) {
            Ok(body) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {parent}"))?;
                }
                std::fs::write(&path, &body).with_context(|| format!("writing {path}"))?;
                println!("{} bytes", body.len());
                fetched += 1;
            }
            Err(error) => {
                println!("failed");
                failed.push(format!("{}: {error}", spec.name));
            }
        }
    }
    println!("fetched {fetched} documents");
    if !failed.is_empty() {
        println!("could not fetch:");
        for line in &failed {
            println!("  {line}");
        }
    }
    Ok(())
}

fn download(url: &str) -> Result<Vec<u8>> {
    // api.weather.gov and several other public endpoints reject requests without a User-Agent.
    // Be honest about what we are.
    let agent = concat!(
        "progeny-corpus/",
        env!("CARGO_PKG_VERSION"),
        " (+https://github.com/romnn/progeny)"
    );
    let mut response = ureq::get(url)
        .header("User-Agent", agent)
        .config()
        .timeout_global(Some(Duration::from_mins(3)))
        .build()
        .call()
        .with_context(|| format!("GET {url}"))?;

    // Published descriptions get large: one is roughly 5 MB and several are bigger.
    let body = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("reading the response body from {url}"))?;

    let head = body.get(..body.len().min(64)).unwrap_or_default();
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    if head.starts_with("<!DOCTYPE") || head.starts_with("<html") {
        bail!("{url} served an HTML page rather than a description");
    }
    Ok(body)
}

fn check(spec: &Spec, args: &Args, totals: &mut Stats, counted: &mut usize) -> Outcome {
    let path = document_path(spec);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return Outcome::Unavailable(format!("{path}: {error}")),
    };

    let result = match harness::round_trip(&bytes) {
        Ok(result) => result,
        Err(error) => return Outcome::Rejected(error.to_string()),
    };
    if !result.is_clean() {
        return Outcome::Differs(result.differences);
    }

    // Determinism: identical input has to produce an identical model. At this stage that means the
    // re-serialized value, which is the only output there is; once renderers exist the same check
    // compares rendered bytes.
    match harness::round_trip(&bytes) {
        Ok(again) if again.reserialized == result.reserialized => {}
        _ => return Outcome::Nondeterministic,
    }

    if args.stats
        && let Ok(stats) = harness::stats(&bytes)
    {
        totals.merge(&stats);
        *counted += 1;
    }

    let version_drift = spec.version.as_deref().and_then(|recorded| {
        let family: String = result
            .declared_version
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        (family != recorded)
            .then(|| format!("the manifest records {recorded}, the document declares {family}"))
    });

    Outcome::Clean {
        schemas: result.schema_count,
        diagnostics: result
            .diagnostics
            .iter()
            .map(progeny::Diagnostic::to_json_line)
            .collect(),
        yaml: result.yaml,
        declared: result.declared_version,
        version_drift,
    }
}

fn report(specs: &[Spec], outcomes: &[(String, Outcome, Duration)], show_diagnostics: bool) {
    println!();
    let mut clean = 0usize;
    let mut unavailable = Vec::new();
    let mut broken = Vec::new();
    let mut drifted = Vec::new();

    for (name, outcome, elapsed) in outcomes {
        let millis = elapsed.as_millis();
        match outcome {
            Outcome::Clean {
                schemas,
                diagnostics,
                yaml,
                declared,
                version_drift,
            } => {
                clean += 1;
                let format = if *yaml { "yaml" } else { "json" };
                let count = diagnostics.len();
                println!(
                    "  ok        {name:<24} {declared:<6} {format}  {schemas:>6} schemas  \
                     {count:>4} diagnostics  {millis:>6} ms"
                );
                if show_diagnostics {
                    for line in diagnostics {
                        println!("              {line}");
                    }
                }
                if let Some(drift) = version_drift {
                    drifted.push(format!("{name}: {drift}"));
                }
            }
            Outcome::Differs(differences) => {
                broken.push(name.clone());
                println!("  DIFFERS   {name:<24} {} differences", differences.len());
                if let Some(notes) = notes_for(specs, name) {
                    println!("              stresses: {notes}");
                }
                for difference in differences.iter().take(5) {
                    println!(
                        "              {} — {}",
                        difference.location, difference.detail
                    );
                }
                if differences.len() > 5 {
                    println!("              … and {} more", differences.len() - 5);
                }
            }
            Outcome::Nondeterministic => {
                broken.push(name.clone());
                println!("  UNSTABLE  {name:<24} two runs disagreed");
            }
            Outcome::Rejected(reason) => {
                broken.push(name.clone());
                println!("  REJECTED  {name:<24} {reason}");
                if let Some(notes) = notes_for(specs, name) {
                    println!("              stresses: {notes}");
                }
            }
            Outcome::Unavailable(reason) => {
                unavailable.push(name.clone());
                println!("  --        {name:<24} unavailable: {reason}");
            }
        }
    }

    println!();
    println!(
        "round-trip: {clean}/{} clean, {} broken, {} unavailable (manifest has {})",
        outcomes.len(),
        broken.len(),
        unavailable.len(),
        specs.len()
    );
    if !unavailable.is_empty() {
        println!(
            "unavailable: {} — run `cargo xtask corpus --fetch`",
            unavailable.join(", ")
        );
    }
    if !drifted.is_empty() {
        println!("the manifest has drifted from what these publishers now serve:");
        for line in &drifted {
            println!("  {line}");
        }
    }
}

/// What a document stresses, from the manifest. Printed with a failure, because knowing that a
/// document is the one with 925 `patternProperties` is most of reading its failure.
fn notes_for<'a>(specs: &'a [Spec], name: &str) -> Option<&'a str> {
    specs
        .iter()
        .find(|spec| spec.name == name)?
        .notes
        .as_deref()
}

fn report_stats(totals: &Stats, documents: usize) {
    println!();
    println!("model-level counts over {documents} documents");
    println!("  schemas                     {}", totals.schemas);
    println!("  deepest schema nesting      {}", totals.max_schema_depth);
    println!("  oneOf                       {}", totals.one_of);
    println!("  with a discriminator        {}", totals.discriminated);
    println!(
        "  patternProperties           {}",
        totals.pattern_properties
    );
    println!("  prefixItems                 {}", totals.prefix_items);
    println!("  const                       {}", totals.constants);
    println!("  anyOf                       {}", totals.any_of.total);
    println!("    nullable emulation        {}", totals.any_of.nullable);
    println!("    an enumeration            {}", totals.any_of.constants);
    println!(
        "    disjoint types            {}",
        totals.any_of.disjoint_types
    );
    println!("    something else            {}", totals.any_of.other);
    println!(
        "  optional and nullable       {}",
        totals.optional_and_nullable
    );
    println!(
        "  integers                    {} ({} carry a bound)",
        totals.integers, totals.bounded_integers
    );
    println!(
        "  multi-media-type bodies     {}",
        totals.multi_content_operations
    );
    println!(
        "  responses with headers      {} ({} headers)",
        totals.responses_with_headers, totals.response_headers
    );
    println!("  external $refs              {}", totals.external_refs);
    println!("  $dynamicRef/$dynamicAnchor  {}", totals.dynamic_scoping);
    println!("  non-root $id                {}", totals.nested_ids);
    let kinds: Vec<String> = totals
        .security_scheme_kinds
        .iter()
        .map(|(kind, count)| format!("{kind}×{count}"))
        .collect();
    println!("  security schemes            {}", kinds.join(", "));
}

fn write_timings(path: &Utf8Path, outcomes: &[(String, Outcome, Duration)]) -> Result<()> {
    let timings: BTreeMap<&str, u128> = outcomes
        .iter()
        .map(|(name, _, elapsed)| (name.as_str(), elapsed.as_millis()))
        .collect();
    let json = serde_json::to_string_pretty(&timings).context("rendering the timings")?;
    std::fs::write(path, json + "\n").with_context(|| format!("writing {path}"))?;
    println!("timings written to {path}");
    Ok(())
}

/// Decide whether the run passed.
///
/// A document that could not be read is infrastructure rather than a finding — one corpus member
/// has no cached copy at all and is fetched live from a vendor endpoint. More than one missing
/// means the cache is not populated, and a run over nothing must not read as a pass.
fn verdict(outcomes: &[(String, Outcome, Duration)]) -> Result<()> {
    let broken = outcomes
        .iter()
        .filter(|(_, outcome, _)| {
            matches!(
                outcome,
                Outcome::Differs(_) | Outcome::Nondeterministic | Outcome::Rejected(_)
            )
        })
        .count();
    let unavailable = outcomes
        .iter()
        .filter(|(_, outcome, _)| matches!(outcome, Outcome::Unavailable(_)))
        .count();

    if broken > 0 {
        bail!("{broken} documents did not round-trip through the model");
    }
    if unavailable > 1 {
        bail!(
            "{unavailable} documents could not be read; run `cargo xtask corpus --fetch` \
             (at most one — the live-only member — may be missing)"
        );
    }
    Ok(())
}
