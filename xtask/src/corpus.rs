//! The conformance corpus: 78 published API descriptions, run through the model.
//!
//! Vendor documents are not committed — they total roughly 117 MB and their redistribution rights
//! vary by publisher — so `corpus/manifest.toml` carries the provenance and `--fetch` downloads
//! each one into a gitignored cache. The single exception is `petstore-31`, which is hand-written,
//! hermetic, and therefore the one document an offline run can always assert against.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use progeny::harness::{self, Stats};
use serde::Deserialize;

use crate::{generated, paths, snapshot};

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

    /// Leave these documents out of whatever was selected, by name. Repeatable.
    ///
    /// For a document a run cannot afford rather than one it does not want. `cloudflare` through
    /// the derive strategy needs about 14.7 GiB for a single rustc invocation, which is more than a
    /// hosted runner has; no job count reaches that, because one unit is already too large.
    #[arg(long = "except", value_name = "NAME")]
    except: Vec<String>,

    /// Report the model-level counts the design questions turn on.
    #[arg(long)]
    stats: bool,

    /// Print every diagnostic each document produced, as JSON lines.
    #[arg(long)]
    show_diagnostics: bool,

    /// Re-record every checked document's diagnostics snapshot instead of comparing against it.
    #[arg(long)]
    pub write_snapshots: bool,

    /// Generate a crate per document and compile it. Slow; the tier CI runs is `--quick --compile`.
    #[arg(long)]
    compile: bool,

    /// Also run clippy over the generated crates, with warnings denied.
    #[arg(long, requires = "compile")]
    clippy: bool,

    /// Write a per-document timing report here, as JSON.
    #[arg(long, value_name = "PATH")]
    timings: Option<Utf8PathBuf>,

    /// Which serde strategy to generate with, rather than the configuration default.
    ///
    /// The escape hatch is only real if somebody runs it, so both modes have to be reachable from
    /// the gate that runs 78 documents. Until stage 8 the hand-written path had met exactly one
    /// 56-line fixture, which is not a claim about the corpus.
    #[arg(long, value_name = "STRATEGY")]
    serde: Option<SerdeChoice>,
}

/// The two serde strategies, as a command-line word.
///
/// Its own enum rather than `progeny::SerdeImpl` because that type is public API deserialized from
/// a configuration file; teaching it to parse a CLI token would put `clap` in the library's
/// dependency list to save one `match`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SerdeChoice {
    /// The derive, always.
    Derive,
    /// The hand-written implementation wherever the eligibility function allows it.
    HandWritten,
}

impl From<SerdeChoice> for progeny::SerdeImpl {
    fn from(choice: SerdeChoice) -> Self {
        match choice {
            SerdeChoice::Derive => Self::DeriveAlways,
            SerdeChoice::HandWritten => Self::HandWrittenWhereEligible,
        }
    }
}

/// One entry of `corpus/manifest.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
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
    /// Boxed because it is an order of magnitude larger than every other outcome, and the enum is
    /// moved around per document.
    Clean(Box<Clean>),
    Differs(Vec<harness::Difference>),
    Nondeterministic,
    Rejected(String),
    /// The document could not be read at all. Not a finding about progeny.
    Unavailable(String),
}

/// Everything a document that came through cleanly has to report.
struct Clean {
    schemas: usize,
    diagnostics: Vec<String>,
    yaml: bool,
    declared: String,
    /// Set when the document declares a different dialect than the manifest recorded, which means
    /// either the manifest went stale or the publisher moved.
    version_drift: Option<String>,
    resolution: harness::Resolution,
    snapshot: snapshot::Verdict,
    /// How much source the document generates, in lines.
    rendered: usize,
    /// What compiling the generated crate found, when it was compiled.
    compiled: Option<generated::Compiled>,
}

pub fn run(args: &Args) -> eyre::Result<()> {
    let specs = load_manifest()?;
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
    if !args.except.is_empty() {
        // Said again, on its own line: an exclusion is a deliberate hole in the coverage, and one
        // name among seventy in the line above is not a hole anybody sees.
        println!("corpus: excluded on request: {}", args.except.join(", "));
    }

    if args.fetch {
        fetch_all(&selected, args.refresh)?;
    }
    if args.compile {
        generated::require_cargo()?;
        println!(
            "corpus: generated crates are written to {} and compiled with a shared target \
             directory",
            generated::scratch_root()
        );
    }

    let convergence_failures = check_convergence()?;

    let mut outcomes: Vec<(String, Outcome, Duration)> = Vec::new();
    let mut totals = Stats::default();
    let mut counted = 0usize;

    for spec in &selected {
        let started = Instant::now();
        let outcome = check(spec, args, &mut totals, &mut counted)?;
        outcomes.push((spec.name.clone(), outcome, started.elapsed()));
    }

    let failures = report(&specs, &outcomes, args.show_diagnostics);
    if args.stats {
        report_stats(&totals, counted);
    }
    if let Some(path) = &args.timings {
        write_timings(path, &outcomes)?;
    }
    if args.write_snapshots {
        let known: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
        for orphan in snapshot::orphans(&known) {
            println!("snapshots: {orphan} has no document in the manifest any more");
        }
    }
    if failures + convergence_failures > 0 {
        bail!("{failures} documents and {convergence_failures} dialect pairs failed");
    }

    verdict(&outcomes)
}

pub fn load_manifest() -> eyre::Result<Vec<Spec>> {
    let path = paths::corpus_root().join("manifest.toml");
    let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("reading {path}"))?;
    let manifest: Manifest = toml::from_str(&text).wrap_err_with(|| format!("parsing {path}"))?;
    // Linted here rather than by one caller: every gate reads this manifest, and a gate trusting
    // an unlinted one once meant a duplicated entry could hand `payloads` a different
    // `bad_examples` list than `corpus` tested with — whichever `.find()` hit first.
    lint_manifest(&manifest.spec)?;
    Ok(manifest.spec)
}

/// One gate's selection, resolved to documents with their bytes.
///
/// The refusal semantics are the corpus gate's, shared so no gate can pass green on nothing: a
/// name the manifest does not list is an error, and a selected document whose bytes are not on
/// disk is an error too. A missing cache is a reason to run `corpus:fetch`, never a silently
/// smaller run — the payloads gate once printed `skipped` per missing file and exited green
/// having checked zero documents.
pub fn selected(names: &[String]) -> eyre::Result<Vec<(Spec, Vec<u8>)>> {
    let specs = load_manifest()?;
    let mut out = Vec::new();
    for name in names {
        let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
            bail!("no corpus document named `{name}`");
        };
        let path = document_path(spec);
        let bytes = std::fs::read(&path)
            .wrap_err_with(|| format!("reading {path}; run `cargo xtask corpus --fetch`"))?;
        out.push((spec.clone(), bytes));
    }
    Ok(out)
}

/// Check the manifest itself before trusting anything it says.
///
/// The two fields that carry hard-won knowledge — which documents ship examples that contradict
/// their own schemas, and what each document stresses — are only useful if they stay accurate, and
/// a duplicated or misplaced entry is the kind of rot nobody notices by reading.
fn lint_manifest(specs: &[Spec]) -> eyre::Result<()> {
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

fn select(specs: &[Spec], args: &Args) -> eyre::Result<Vec<Spec>> {
    // A misspelled exclusion excludes nothing and reports a full run, which is the one reading a
    // coverage gate must never produce.
    for name in &args.except {
        if !specs.iter().any(|spec| &spec.name == name) {
            bail!("`--except {name}` names no document in the manifest");
        }
    }
    let selected = if !args.only.is_empty() {
        let mut selected = Vec::new();
        for name in &args.only {
            let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
                bail!("no corpus document named `{name}`");
            };
            selected.push(spec.clone());
        }
        selected
    } else if args.quick {
        let mut selected = Vec::new();
        for name in &quick_tier()? {
            let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
                bail!("the quick tier names `{name}`, which is not in the manifest");
            };
            selected.push(spec.clone());
        }
        selected
    } else {
        specs.to_vec()
    };
    Ok(selected
        .into_iter()
        .filter(|spec| !args.except.contains(&spec.name))
        .collect())
}

/// Where a document lives on disk.
///
/// A committed document lives in `corpus/specs`; everything else in the cache. The cached
/// filename is the manifest's when it has one, and otherwise the document's name with an
/// extension guessed from the URL — several corpus documents are served from extensionless URLs,
/// and one of those serves YAML from a name that ends up `.json`, which is why nothing downstream
/// may dispatch on the extension.
pub fn document_path(spec: &Spec) -> Utf8PathBuf {
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

fn fetch_all(specs: &[Spec], refresh: bool) -> eyre::Result<()> {
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
                        .wrap_err_with(|| format!("creating {parent}"))?;
                }
                std::fs::write(&path, &body).wrap_err_with(|| format!("writing {path}"))?;
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

fn download(url: &str) -> eyre::Result<Vec<u8>> {
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
        .wrap_err_with(|| format!("GET {url}"))?;

    // Published descriptions get large: one is roughly 5 MB and several are bigger.
    let body = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .wrap_err_with(|| format!("reading the response body from {url}"))?;

    let head = body.get(..body.len().min(64)).unwrap_or_default();
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    if head.starts_with("<!DOCTYPE") || head.starts_with("<html") {
        bail!("{url} served an HTML page rather than a description");
    }
    Ok(body)
}

/// Check every 3.0/3.1 pair, and say how many disagreed.
///
/// A pair is two hand-written documents describing the same API in the two dialects. They are
/// committed and tiny, so this runs offline and on every invocation: the round trip proves the
/// model holds what a document said, and only this proves that what the *normalizer* said it said
/// is right.
fn check_convergence() -> eyre::Result<usize> {
    let root = paths::convergence_root();
    // Name to extension, from the file actually found — the pair is opened with the same
    // extension discovery matched, where a discovery that accepted any `.3.0.<ext>` and an open
    // step that hardcoded `.yaml` once meant a committed `.3.0.json` failed the whole gate with
    // "reading …3.0.yaml" instead of being tested.
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => bail!("{root}: {error}"),
    };
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if let Some((name, extension)) = path.file_name().and_then(|file| file.split_once(".3.0."))
        {
            pairs.insert(name.to_owned(), extension.to_owned());
        }
    }
    if pairs.is_empty() {
        bail!("{root} holds no `<name>.3.0.<ext>` document");
    }

    let mut failures = 0usize;
    for (name, extension) in &pairs {
        let (old, new) = (
            root.join(format!("{name}.3.0.{extension}")),
            root.join(format!("{name}.3.1.{extension}")),
        );
        let old_bytes = std::fs::read(&old).wrap_err_with(|| format!("reading {old}"))?;
        let new_bytes = std::fs::read(&new).wrap_err_with(|| {
            format!("reading {new}; a `.3.0.` document needs its `.3.1.` counterpart beside it")
        })?;
        match harness::convergence(&old_bytes, &new_bytes) {
            Ok(result) if result.is_clean() => {
                println!("  ok        {name:<24} the two dialects agree, model and source");
            }
            Ok(result) => {
                failures += 1;
                // Named separately because they mean different things: a model difference is a
                // normalization defect, and a source difference with the models agreeing is a
                // defect in a stage after it.
                println!(
                    "  DIFFERS   {name:<24} {} in the model, {} in the generated source, {} \
                     one-sided losses",
                    result.differences.len(),
                    result.output.len(),
                    result.asymmetric.len()
                );
                for difference in result
                    .differences
                    .iter()
                    .chain(&result.output)
                    .chain(&result.asymmetric)
                    .take(5)
                {
                    println!(
                        "              {} — {}",
                        difference.location, difference.detail
                    );
                }
            }
            Err(error) => {
                failures += 1;
                println!("  REJECTED  {name:<24} {error}");
            }
        }
    }
    println!(
        "convergence: {}/{} dialect pairs agree",
        pairs.len() - failures,
        pairs.len()
    );
    Ok(failures)
}

fn check(
    spec: &Spec,
    args: &Args,
    totals: &mut Stats,
    counted: &mut usize,
) -> eyre::Result<Outcome> {
    let path = document_path(spec);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return Ok(Outcome::Unavailable(format!("{path}: {error}"))),
    };

    let result = match harness::round_trip(&bytes) {
        Ok(result) => result,
        Err(error) => return Ok(Outcome::Rejected(error.to_string())),
    };
    if !result.is_clean() {
        return Ok(Outcome::Differs(result.differences));
    }

    // Determinism: identical input has to produce an identical model. At this stage that means the
    // re-serialized value, which is the only output there is; once renderers exist the same check
    // compares rendered bytes.
    match harness::round_trip(&bytes) {
        Ok(again) if again.reserialized == result.reserialized => {}
        _ => return Ok(Outcome::Nondeterministic),
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

    // Generation, not just the round trip, is what the snapshot records: the shape and contract
    // layers have plenty to say, and a snapshot that only covered the front end would go quiet
    // exactly where the interesting decisions moved to.
    let mut config = config_for(spec);
    if let Some(choice) = args.serde {
        config.serde_impl = choice.into();
    }
    let output = match progeny::generate(&bytes, &config) {
        Ok(output) => output,
        Err(error) => return Ok(Outcome::Rejected(error.to_string())),
    };
    match progeny::generate(&bytes, &config) {
        Ok(again) if again.files == output.files => {}
        _ => return Ok(Outcome::Nondeterministic),
    }
    let rendered = output
        .files
        .values()
        .map(|contents| contents.lines().count())
        .sum();
    let compiled = if args.compile {
        let directory = generated::write(&spec.name, &output)?;
        Some(generated::check(&directory, args.clippy)?)
    } else {
        None
    };

    let diagnostics: Vec<String> = output
        .diagnostics
        .iter()
        .map(progeny::Diagnostic::to_json_line)
        .collect();
    let taken = snapshot::Snapshot::take(&bytes, &diagnostics);
    let verdict = if args.write_snapshots {
        snapshot::write(&spec.name, &taken)?;
        snapshot::Verdict::Match
    } else {
        taken.compare(snapshot::read(&spec.name).as_ref())
    };

    Ok(Outcome::Clean(Box::new(Clean {
        schemas: result.schema_count,
        diagnostics,
        yaml: result.yaml,
        declared: result.declared_version,
        version_drift,
        resolution: result.resolution,
        snapshot: verdict,
        rendered,
        compiled,
    })))
}

/// The configuration the corpus generates with.
///
/// Deliberately the defaults, plus a crate name: the corpus measures what a caller gets without
/// having to know anything, and a gate that only passes under a tuned configuration is not a gate.
/// The library name cargo derives from a package name.
///
/// Cargo's own rule, spelled once: three gates re-derived it independently before, which is
/// three chances to disagree with cargo about what `use corpus-x::…` is called.
pub fn lib_name(package: &str) -> String {
    package.replace('-', "_")
}

pub fn config_for(spec: &Spec) -> progeny::Config {
    progeny::Config {
        package: progeny::Package {
            name: format!("corpus-{}", spec.name),
            version: "0.0.0".to_owned(),
        },
        ..progeny::Config::default()
    }
}

/// The documents `corpus/tier.toml` names as the quick tier.
///
/// The expensive per-document assertions — compiling a generated crate, measuring what it costs —
/// run over this list rather than over all 78, and the caller says so out loud rather than
/// sampling silently.
pub fn quick_tier() -> eyre::Result<Vec<String>> {
    let path = paths::corpus_root().join("tier.toml");
    let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("reading {path}"))?;
    let tier: Tier = toml::from_str(&text).wrap_err_with(|| format!("parsing {path}"))?;
    Ok(tier.quick)
}

/// Print the per-document lines and the summary, and count the documents that failed.
fn report(
    specs: &[Spec],
    outcomes: &[(String, Outcome, Duration)],
    show_diagnostics: bool,
) -> usize {
    println!();
    let mut totals = Totals::default();
    for (name, outcome, elapsed) in outcomes {
        match outcome {
            Outcome::Clean(clean) => {
                describe_clean(name, clean, elapsed, show_diagnostics, &mut totals);
            }
            Outcome::Differs(differences) => {
                totals.broken.push(name.clone());
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
                totals.broken.push(name.clone());
                println!("  UNSTABLE  {name:<24} two runs disagreed");
            }
            Outcome::Rejected(reason) => {
                totals.broken.push(name.clone());
                println!("  REJECTED  {name:<24} {reason}");
                if let Some(notes) = notes_for(specs, name) {
                    println!("              stresses: {notes}");
                }
            }
            Outcome::Unavailable(reason) => {
                totals.unavailable.push(name.clone());
                println!("  --        {name:<24} unavailable: {reason}");
            }
        }
    }
    summarize(specs, outcomes.len(), &totals);
    totals.broken.len()
}

/// What the run adds up to across every document.
#[derive(Default)]
struct Totals {
    clean: usize,
    broken: Vec<String>,
    unavailable: Vec<String>,
    drifted: Vec<String>,
    snapshots: Vec<(String, snapshot::Verdict)>,
    references: harness::Resolution,
    rendered: usize,
    compiled: usize,
    compile_failures: usize,
}

/// The line one document that came through cleanly prints, and what it adds to the totals.
fn describe_clean(
    name: &str,
    clean: &Clean,
    elapsed: &Duration,
    show_diagnostics: bool,
    totals: &mut Totals,
) {
    let millis = elapsed.as_millis();
    let Clean {
        schemas,
        diagnostics,
        yaml,
        declared,
        version_drift,
        resolution,
        snapshot,
        rendered,
        compiled,
    } = clean;
    totals.clean += 1;
    totals.rendered += rendered;
    merge_resolution(&mut totals.references, resolution);

    let format = if *yaml { "yaml" } else { "json" };
    let count = diagnostics.len();
    let refs = resolution.references;
    let unresolved = resolution.dangling + resolution.external;
    let failed_to_compile = compiled.as_ref().is_some_and(|it| !it.ok);
    let mut flag = if snapshot.is_failure() {
        " SNAPSHOT".to_owned()
    } else {
        String::new()
    };
    if failed_to_compile {
        flag.push_str(" DOES NOT COMPILE");
        totals.broken.push(name.to_owned());
        totals.compile_failures += 1;
    }
    println!(
        "  ok        {name:<24} {declared:<6} {format}  {schemas:>6} schemas  {refs:>6} refs  \
         {unresolved:>3} unresolved  {count:>3} diagnostics  {rendered:>7} lines  \
         {millis:>6} ms{flag}"
    );
    if let Some(compiled) = compiled {
        totals.compiled += 1;
        if !compiled.ok {
            println!("              {}", compiled.complaint);
        }
    }
    if show_diagnostics {
        for line in diagnostics {
            println!("              {line}");
        }
    }
    if let Some(drift) = version_drift {
        totals.drifted.push(format!("{name}: {drift}"));
    }
    if *snapshot != snapshot::Verdict::Match {
        if snapshot.is_failure() {
            totals.broken.push(name.to_owned());
        }
        totals.snapshots.push((name.to_owned(), snapshot.clone()));
    }
}

fn summarize(specs: &[Spec], ran: usize, totals: &Totals) {
    let (clean, references) = (totals.clean, &totals.references);
    println!();
    println!(
        "round-trip: {clean}/{ran} clean, {} broken, {} unavailable (manifest has {})",
        totals.broken.len(),
        totals.unavailable.len(),
        specs.len()
    );
    if !totals.unavailable.is_empty() {
        println!(
            "unavailable: {} — run `cargo xtask corpus --fetch`",
            totals.unavailable.join(", ")
        );
    }
    println!(
        "references: {} schema refs, {} resolved, {} repaired, {} dangling, {} external, {} \
         dynamic; {} component refs, {} dangling",
        references.references,
        references.resolved,
        references.repaired,
        references.dangling,
        references.external,
        references.dynamic,
        references.component_references,
        references.dangling_components,
    );
    println!(
        "cycles: {} groups of mutually referencing schemas, largest {}",
        references.recursive_groups, references.largest_recursive_group
    );
    println!(
        "rendered: {} lines of Rust across {clean} documents",
        totals.rendered
    );
    if totals.compiled > 0 {
        println!(
            "compiled: {}/{} generated crates check clean",
            totals.compiled - totals.compile_failures,
            totals.compiled
        );
    }
    if !totals.drifted.is_empty() {
        println!("the manifest has drifted from what these publishers now serve:");
        for line in &totals.drifted {
            println!("  {line}");
        }
    }
    if totals.snapshots.is_empty() {
        println!("snapshots: {clean}/{clean} match");
        return;
    }
    println!("snapshots:");
    for (name, verdict) in &totals.snapshots {
        println!(
            "  {name}: {} ({})",
            verdict.headline(),
            snapshot::display(name)
        );
        if let snapshot::Verdict::Regressed { added, removed } = verdict {
            for line in added {
                println!("      + {line}");
            }
            for line in removed {
                println!("      - {line}");
            }
        }
    }
    println!("re-record with `cargo xtask regen-snapshots`, after reading the diff");
}

/// Fold one document's reference counts into the corpus totals.
fn merge_resolution(totals: &mut harness::Resolution, one: &harness::Resolution) {
    totals.references += one.references;
    totals.resolved += one.resolved;
    totals.repaired += one.repaired;
    totals.dangling += one.dangling;
    totals.external += one.external;
    totals.dynamic += one.dynamic;
    totals.component_references += one.component_references;
    totals.dangling_components += one.dangling_components;
    totals.recursive_groups += one.recursive_groups;
    totals.largest_recursive_group = totals
        .largest_recursive_group
        .max(one.largest_recursive_group);
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

fn write_timings(path: &Utf8Path, outcomes: &[(String, Outcome, Duration)]) -> eyre::Result<()> {
    let timings: BTreeMap<&str, u128> = outcomes
        .iter()
        .map(|(name, _, elapsed)| (name.as_str(), elapsed.as_millis()))
        .collect();
    let json = serde_json::to_string_pretty(&timings).wrap_err("rendering the timings")?;
    std::fs::write(path, json + "\n").wrap_err_with(|| format!("writing {path}"))?;
    println!("timings written to {path}");
    Ok(())
}

/// Decide whether the run passed.
///
/// A document that could not be read is infrastructure rather than a finding — one corpus member
/// has no cached copy at all and is fetched live from a vendor endpoint. More than one missing
/// means the cache is not populated, and a run over nothing must not read as a pass.
fn verdict(outcomes: &[(String, Outcome, Duration)]) -> eyre::Result<()> {
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

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use color_eyre::eyre::{self, OptionExt as _};

    use super::{Args, Spec, select};

    /// `Args` is a subcommand's argument group, so parsing it needs something to hang it on.
    #[derive(Debug, clap::Parser)]
    struct Invocation {
        #[command(flatten)]
        args: Args,
    }

    fn parse(arguments: &[&str]) -> Args {
        Invocation::parse_from(std::iter::once("corpus").chain(arguments.iter().copied())).args
    }

    fn manifest(names: &[&str]) -> eyre::Result<Vec<Spec>> {
        names
            .iter()
            .map(|name| {
                toml::from_str(&indoc::formatdoc! {r#"
                    name = "{name}"
                    local = true
                "#})
                .map_err(Into::into)
            })
            .collect()
    }

    #[test_util::test]
    fn an_excluded_document_is_left_out_of_what_was_selected() {
        let specs = manifest(&["petstore-31", "cloudflare", "oxide"])?;
        let selected = select(
            &specs,
            &parse(&[
                "--only",
                "petstore-31",
                "--only",
                "cloudflare",
                "--except",
                "cloudflare",
            ]),
        )?;
        let names: Vec<&str> = selected.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ["petstore-31"]);
    }

    /// The failure mode this guards is not a missing document but a *reported* one: a misspelled
    /// exclusion silently excludes nothing, and the run then claims coverage it did not have.
    #[test_util::test]
    fn an_exclusion_that_names_nothing_is_refused() {
        let specs = manifest(&["petstore-31", "cloudflare"])?;
        let error = select(&specs, &parse(&["--except", "petstore-30"]))
            .err()
            .ok_or_eyre("a name no document carries cannot be excluded")?;
        assert!(error.to_string().contains("petstore-30"), "{error}");
    }
}
