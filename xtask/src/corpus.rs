//! The conformance corpus: 78 published API descriptions, run through the model.
//!
//! Vendor documents are not committed — they total roughly 117 MB and their redistribution rights
//! vary by publisher — so `corpus/manifest.toml` carries the provenance and `--fetch` downloads
//! each one into a gitignored cache.
//! The exceptions are the `local` documents in `corpus/specs`: `petstore-31` and a few fixtures,
//! hand-written and hermetic, and therefore the documents an offline run can always assert against.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr, bail};
use progeny::harness::{self, Stats};
use serde::Deserialize;

use crate::{generated, jobs, paths, snapshot};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Download every document listed in the manifest that is not already cached, then stop.
    #[arg(long)]
    fetch: bool,

    /// Re-download even live documents that are already cached; a pinned one is downloaded only
    /// when its cached copy does not verify.
    ///
    /// A download that fails keeps the cached copy, which is what lets CI fall back to its last
    /// good corpus when a publisher is down or has moved.
    #[arg(long, requires = "fetch")]
    refresh: bool,

    /// Run only the quick tier from `corpus/tier.toml`.
    #[arg(long)]
    quick: bool,

    /// Run only documents whose bytes cannot change: committed ones, and ones fetched at a
    /// recorded commit. What pull requests and `main` run, so a publisher cannot change a result.
    #[arg(long)]
    pinned: bool,

    /// Run only these documents, by name. Repeatable.
    #[arg(long = "only", value_name = "NAME")]
    only: Vec<String>,

    /// Leave these documents out of whatever was selected, by name. Repeatable.
    ///
    /// For a document a particular run cannot afford rather than one it does not want. Every
    /// exclusion is printed, and no standard gate supplies one implicitly.
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

    /// Check each generated crate under every feature selection a consumer can make.
    ///
    /// All features, no default features, and each edge feature the crate declares (`client`,
    /// `server`) alone.
    /// Only crate packaging has those features, so any other packaging is refused.
    #[arg(long, requires = "compile")]
    feature_matrix: bool,

    /// Write a per-document timing report here, as JSON.
    #[arg(long, value_name = "PATH")]
    timings: Option<Utf8PathBuf>,

    /// How many documents to check at once.
    ///
    /// Defaults to four, or fewer when the cores or the free memory cannot carry four: about
    /// 4 GiB per document to generate, and 10 GiB to compile one.
    /// Each compiles in its own cargo target directory, because cargo locks one for a whole build.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    /// Which serde strategy to generate with, rather than the configuration default.
    ///
    /// The escape hatch is only real if somebody runs it, so both modes have to be reachable from
    /// the gate that runs 78 documents. Until stage 8 the hand-written path had met exactly one
    /// 56-line fixture, which is not a claim about the corpus.
    #[arg(long, value_name = "STRATEGY")]
    serde: Option<SerdeChoice>,

    /// Which ahead-of-time packaging to compile. Ordinary corpus runs keep the crate default.
    #[arg(long, value_name = "PACKAGING", requires = "compile")]
    packaging: Option<PackagingChoice>,

    /// How the generated client reads responses, rather than the configuration default.
    ///
    /// Strict decoding ships a different client runtime — no read forms, no decoder, and under
    /// workspace packaging a report of its own — so a gate that only compiles the default is
    /// blind to what a strict client warns about.
    #[arg(long, value_name = "DECODING", requires = "compile")]
    decoding: Option<DecodingChoice>,

    /// Build the generated crates on this rustup toolchain, such as `1.88`, instead of the one
    /// running xtask. xtask itself needs a newer Rust than the generated code does.
    #[arg(long, value_name = "NAME", requires = "compile")]
    toolchain: Option<String>,
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PackagingChoice {
    Crate,
    Workspace,
}

impl From<PackagingChoice> for progeny::Packaging {
    fn from(choice: PackagingChoice) -> Self {
        match choice {
            PackagingChoice::Crate => Self::Crate,
            PackagingChoice::Workspace => Self::Workspace,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DecodingChoice {
    Lenient,
    Strict,
}

impl From<DecodingChoice> for progeny::Decoding {
    fn from(choice: DecodingChoice) -> Self {
        match choice {
            DecodingChoice::Lenient => Self::Lenient,
            DecodingChoice::Strict => Self::Strict,
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
    /// The commit a `raw.githubusercontent.com` document is fetched at, in place of the branch
    /// its `url` names. Moving it is a reviewed change, with the re-recorded snapshot beside it.
    rev: Option<String>,
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

impl Spec {
    /// Whether the document's bytes cannot change under us: committed, or fetched at a commit.
    pub fn pinned(&self) -> bool {
        self.local || self.rev.is_some()
    }

    /// Where to download the document: its `url`, with the branch replaced by `rev` when pinned.
    fn download_url(&self) -> Option<String> {
        let url = self.url.as_deref()?;
        let Some(rev) = &self.rev else {
            return Some(url.to_owned());
        };
        // `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/<path>`
        let rest = url.strip_prefix(GITHUB_RAW)?;
        let mut segments = rest.splitn(4, '/');
        let (owner, repo, _branch, path) = (
            segments.next()?,
            segments.next()?,
            segments.next()?,
            segments.next()?,
        );
        Some(format!("{GITHUB_RAW}{owner}/{repo}/{rev}/{path}"))
    }
}

const GITHUB_RAW: &str = "https://raw.githubusercontent.com/";

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
    require_committed(&specs)?;
    let selected = select(&specs, args)?;
    require_crate_packaging(args)?;

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
        // Downloading is all this flag does. Falling through into the run cost CI's fetch step
        // three minutes of a corpus that the step after it runs again.
        return Ok(());
    }
    if args.compile {
        generated::require_cargo()?;
        println!(
            "corpus: generated crates are written to {} and compiled with a shared target \
             directory",
            generated::scratch_root()
        );
    }
    if args.feature_matrix {
        println!(
            "corpus: each generated crate is checked with all features, with no default features, \
             and with each edge feature it declares alone"
        );
    }

    let convergence_failures = check_convergence()?;

    let mut outcomes: Vec<(String, Outcome, Duration)> = Vec::new();
    let mut totals = Stats::default();
    let mut counted = 0usize;

    // Documents run at once up to the job count, and report in manifest order whatever it is.
    let budget = match (args.compile, args.serde) {
        (false, _) => jobs::Budget::Generate,
        (true, Some(SerdeChoice::Derive)) => jobs::Budget::CompileDerive,
        (true, _) => jobs::Budget::Compile,
    };
    let jobs = jobs::resolve(args.jobs, budget);
    println!("corpus: {jobs} documents at a time");
    let checked = jobs::map_ordered(&selected, jobs, |spec, slot| {
        let started = Instant::now();
        check(spec, args, slot).map(|checked| (checked, started.elapsed()))
    })?;
    for (spec, checked) in selected.iter().zip(checked) {
        let ((outcome, stats), elapsed) = checked?;
        if let Some(stats) = stats {
            print_upload_stats(&spec.name, &stats);
            totals.merge(&stats);
            counted += 1;
        }
        outcomes.push((spec.name.clone(), outcome, elapsed));
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

    verdict(&specs, &args.only, &outcomes)
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
    // A document named twice is checked once, for the reason [`select`] gives.
    let mut seen = std::collections::BTreeSet::new();
    for name in names.iter().filter(|name| seen.insert(name.as_str())) {
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

/// Refuse the feature matrix under any packaging but a single crate.
///
/// Only a crate gates its client and server behind features; a workspace splits them into crates
/// of their own, and a module is compiled under its includer's features.
fn require_crate_packaging(args: &Args) -> eyre::Result<()> {
    let packaging = args
        .packaging
        .map_or_else(progeny::Packaging::default, Into::into);
    if args.feature_matrix && packaging != progeny::Packaging::Crate {
        bail!(
            "`--feature-matrix` checks a crate's `client` and `server` features, which only crate \
             packaging has; drop `--packaging` or pass `--packaging crate`"
        );
    }
    Ok(())
}

/// Fail unless every committed document can be read, whether this run selected it or not.
///
/// A committed document is part of the checkout, so one that cannot be read is a broken tree
/// rather than a download that failed, and nothing a later run fetches will bring it back.
/// The fixtures among them are what the compile gates are pointed at to pin a shape, and one
/// that went missing would otherwise let those gates pass on what was left.
fn require_committed(specs: &[Spec]) -> eyre::Result<()> {
    for spec in specs.iter().filter(|spec| spec.local) {
        let path = document_path(spec);
        std::fs::read(&path)
            .wrap_err_with(|| format!("reading {path}, which `{}` commits", spec.name))?;
    }
    Ok(())
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
        if let Some(rev) = &spec.rev {
            let commit = rev.len() == 40 && rev.bytes().all(|byte| byte.is_ascii_hexdigit());
            if !commit || spec.download_url().is_none() {
                bail!(
                    "`{}` pins `{rev}`, but a rev is a full commit sha on a {GITHUB_RAW} url",
                    spec.name
                );
            }
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
    let selected = if args.pinned {
        selected.into_iter().filter(Spec::pinned).collect()
    } else {
        selected
    };
    // A document named twice is checked once: two workers writing one generated directory would
    // race, and a repeat checks nothing new.
    let mut seen = std::collections::BTreeSet::new();
    Ok(selected
        .into_iter()
        .filter(|spec| !args.except.contains(&spec.name) && seen.insert(spec.name.clone()))
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
    let mut pinned_failed = Vec::new();
    for spec in specs {
        let path = document_path(spec);
        // A pinned document must be the one its snapshot was taken from, so a cached copy that is
        // not is fetched again, and a download that is not either fails the run. A pinned document
        // without a snapshot has nothing to be checked against, and fails too.
        let recorded = if spec.pinned() {
            let Some(recorded) = snapshot::read(&spec.name) else {
                pinned_failed.push(format!("{}: no snapshot to check it against", spec.name));
                continue;
            };
            Some(recorded)
        } else {
            None
        };
        let current = |bytes: &[u8]| {
            recorded
                .as_ref()
                .is_none_or(|snapshot| snapshot.is_of(bytes))
        };
        let cached = std::fs::read(&path).is_ok_and(|bytes| current(&bytes));
        if spec.local {
            // Committed, so there is nothing to download: an edit without a re-recorded snapshot.
            if !cached {
                pinned_failed.push(format!(
                    "{}: not the document its snapshot was taken from",
                    spec.name
                ));
            }
            continue;
        }
        // A pinned copy that verifies is the document, so only live ones are refreshed.
        if cached && (spec.pinned() || !refresh) {
            continue;
        }
        let Some(url) = spec.download_url() else {
            failed.push(format!("{}: no url in the manifest", spec.name));
            continue;
        };
        let url = url.as_str();
        print!("fetching {} … ", spec.name);
        // A download replaces the cached copy only once it reads as a description, so a refresh
        // that gets an error page with a 200 keeps the last good copy, as a failed request does.
        let readable = download(url).and_then(|body| match harness::stats(&body) {
            Ok(_) => Ok(body),
            Err(error) => Err(eyre::eyre!("{url} served no readable description: {error}")),
        });
        let readable = readable.and_then(|body| {
            if current(&body) {
                Ok(body)
            } else {
                Err(eyre::eyre!(
                    "{url} is not the document its snapshot was taken from"
                ))
            }
        });
        match readable {
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
                // The alternate form carries the cause, such as the status a server answered.
                if spec.pinned() {
                    pinned_failed.push(format!("{}: {error:#}", spec.name));
                } else {
                    failed.push(format!("{}: {error:#}", spec.name));
                }
            }
        }
    }
    println!("fetched {fetched} documents");
    if !failed.is_empty() {
        println!("could not fetch:");
        // Under GitHub Actions each failure is also an annotation, because a failure that kept a
        // cached copy passes every gate and would otherwise be read by nobody.
        let annotate = std::env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true");
        for line in &failed {
            println!("  {line}");
            if annotate {
                println!("::warning title=corpus fetch::{line}");
            }
        }
    }
    // A pinned document can always be fetched again, so one that could not be is a broken pin,
    // and the gates must not run without it.
    if !pinned_failed.is_empty() {
        bail!(
            "could not fetch pinned documents:\n  {}",
            pinned_failed.join("\n  ")
        );
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
    // One attempt is the request and the whole body, so a transfer cut off midway is retried too.
    // Three attempts of at most 55 s, with the sleeps between them, stay within three minutes.
    let attempt = || {
        let mut response = ureq::get(url)
            .header("User-Agent", agent)
            .config()
            .timeout_global(Some(Duration::from_secs(55)))
            .build()
            .call()?;
        // Published descriptions get large: one is roughly 5 MB and several are bigger.
        response
            .body_mut()
            .with_config()
            .limit(256 * 1024 * 1024)
            .read_to_vec()
    };
    let backoff = backon::ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(1))
        .with_max_delay(Duration::from_secs(2))
        .with_max_times(2)
        .with_jitter();
    let body = backon::BlockingRetryable::retry(attempt, backoff)
        .when(transient)
        .call()
        .wrap_err_with(|| format!("GET {url}"))?;
    // A `.gz` source is a compressed file rather than a compressed transfer, so no
    // `Content-Encoding` says so and the cache keeps the description itself.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let compressed = path
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("gz"));
    let body = if compressed {
        let mut description = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(body.as_slice()),
            &mut description,
        )
        .wrap_err_with(|| format!("decompressing {url}"))?;
        description
    } else {
        body
    };

    let head = body.get(..body.len().min(64)).unwrap_or_default();
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    if head.starts_with("<!DOCTYPE") || head.starts_with("<html") {
        bail!("{url} served an HTML page rather than a description");
    }
    Ok(body)
}

/// Whether a download that failed may succeed when tried again: no response, a server error, a
/// rate limit, or a connection that dropped mid-transfer.
///
/// A missing host, a body over the limit and a malformed response fail at once, because trying
/// again gets the same answer.
fn transient(error: &ureq::Error) -> bool {
    use std::io::ErrorKind;
    let dropped = |error: &std::io::Error| {
        matches!(
            error.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionRefused
                | ErrorKind::BrokenPipe
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::TimedOut
                | ErrorKind::UnexpectedEof
        )
    };
    match error {
        ureq::Error::StatusCode(status) => *status == 429 || (500..=599).contains(status),
        ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed => true,
        // The transparent gzip reader wraps a transfer cut off midway.
        ureq::Error::Io(error) | ureq::Error::Decompress(_, error) => dropped(error),
        _ => false,
    }
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

/// Check one document, and collect its model-level counts when the run reports them.
///
/// `slot` is the worker running it, whose own cargo target directory a compile builds in.
fn check(spec: &Spec, args: &Args, slot: usize) -> eyre::Result<(Outcome, Option<Stats>)> {
    let path = document_path(spec);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return Ok((Outcome::Unavailable(format!("{path}: {error}")), None)),
    };

    let result = match harness::round_trip(&bytes) {
        Ok(result) => result,
        Err(error) => return Ok((Outcome::Rejected(error.to_string()), None)),
    };
    if !result.is_clean() {
        return Ok((Outcome::Differs(result.differences), None));
    }

    // Determinism: identical input has to produce an identical model. At this stage that means the
    // re-serialized value, which is the only output there is; once renderers exist the same check
    // compares rendered bytes.
    match harness::round_trip(&bytes) {
        Ok(again) if again.reserialized == result.reserialized => {}
        _ => return Ok((Outcome::Nondeterministic, None)),
    }

    let stats = if args.stats {
        harness::stats(&bytes).ok()
    } else {
        None
    };

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
    if let Some(packaging) = args.packaging {
        config.packaging = packaging.into();
    }
    if let Some(decoding) = args.decoding {
        config.decoding = decoding.into();
    }
    let output = match progeny::generate(&bytes, &config) {
        Ok(output) => output,
        Err(error) => return Ok((Outcome::Rejected(error.to_string()), stats)),
    };
    match progeny::generate(&bytes, &config) {
        Ok(again) if again.files == output.files => {}
        _ => return Ok((Outcome::Nondeterministic, stats)),
    }
    let rendered = output
        .files
        .values()
        .map(|contents| contents.lines().count())
        .sum();
    let compiled = if args.compile {
        let directory = generated::write(&spec.name, &output)?;
        let compiled = if args.feature_matrix {
            generated::check_matrix(&directory, args.clippy, slot, args.toolchain.as_deref())?
        } else {
            generated::check(
                &directory,
                args.clippy,
                slot,
                generated::Features::All,
                args.toolchain.as_deref(),
            )?
        };
        generated::clean(&directory, slot, args.toolchain.as_deref())?;
        Some(compiled)
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

    let clean = Outcome::Clean(Box::new(Clean {
        schemas: result.schema_count,
        diagnostics,
        yaml: result.yaml,
        declared: result.declared_version,
        version_drift,
        resolution: result.resolution,
        snapshot: verdict,
        rendered,
        compiled,
    }));
    Ok((clean, stats))
}

/// The response and upload bodies one document declares, when it has any to speak of.
fn print_upload_stats(name: &str, stats: &Stats) {
    if stats.text_response_arms + stats.byte_response_arms > 0 {
        println!(
            "  response bodies: {name}: {} text, {} bytes",
            stats.text_response_arms, stats.byte_response_arms
        );
    }
    if stats.byte_request_bodies + stats.multipart_file_request_bodies > 0 {
        println!(
            "  upload bodies: {name}: {} raw bytes, {} multipart with {} file parts",
            stats.byte_request_bodies,
            stats.multipart_file_request_bodies,
            stats.multipart_file_parts
        );
    }
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
    // Pull requests compile the tier, so a document a publisher can change would change their
    // result.
    let specs = load_manifest()?;
    for name in &tier.quick {
        if !specs.iter().any(|spec| &spec.name == name && spec.pinned()) {
            bail!("the quick tier names `{name}`, which is not a pinned document in the manifest");
        }
    }
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
            for line in compiled.complaint.lines() {
                println!("              {line}");
            }
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
    println!("  active value constraints");
    for (keyword, occurrences) in &totals.value_constraints {
        let documents = totals
            .value_constraint_documents
            .get(keyword)
            .copied()
            .unwrap_or_default();
        println!("    {keyword:<22} {occurrences:>7} in {documents:>2} documents");
    }
    println!(
        "    carried as fixed arrays   {:>7}",
        totals.fixed_array_constraints
    );
    println!("  IP string formats");
    for (format, occurrences) in &totals.ip_formats {
        let documents = totals
            .ip_format_documents
            .get(format)
            .copied()
            .unwrap_or_default();
        println!("    {format:<22} {occurrences:>7} in {documents:>2} documents");
    }
    println!(
        "  multi-media-type bodies     {}",
        totals.multi_content_operations
    );
    println!(
        "  responses with headers      {} ({} headers)",
        totals.responses_with_headers, totals.response_headers
    );
    println!(
        "  selected response arms      {} JSON, {} text, {} bytes, {} empty",
        totals.json_response_arms,
        totals.text_response_arms,
        totals.byte_response_arms,
        totals.empty_response_arms
    );
    println!(
        "  type forms                  {} strict, {} shared, {} read-only, {} twinned",
        totals.forms.strict, totals.forms.shared, totals.forms.read_only, totals.forms.twinned
    );
    println!(
        "  connection upgrades         {} operations declare status 101",
        totals.upgrade_operations
    );
    println!(
        "  large-upload candidates     {} raw-byte bodies, {} multipart bodies with {} file parts",
        totals.byte_request_bodies,
        totals.multipart_file_request_bodies,
        totals.multipart_file_parts
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
/// A fetched document that could not be read is infrastructure rather than a finding — one corpus
/// member has no cached copy at all and is fetched live from a vendor endpoint.
/// More than one missing means the cache is not populated, and a run over nothing must not read as
/// a pass.
///
/// The tolerance covers nothing the run cannot do without.
/// A committed document is part of the checkout, and a document named with `--only` is the whole
/// point of the run, so either one unreadable fails it.
fn verdict(
    specs: &[Spec],
    only: &[String],
    outcomes: &[(String, Outcome, Duration)],
) -> eyre::Result<()> {
    let broken = outcomes
        .iter()
        .filter(|(_, outcome, _)| {
            matches!(
                outcome,
                Outcome::Differs(_) | Outcome::Nondeterministic | Outcome::Rejected(_)
            )
        })
        .count();
    let (required, unavailable): (Vec<&str>, Vec<&str>) = outcomes
        .iter()
        .filter(|(_, outcome, _)| matches!(outcome, Outcome::Unavailable(_)))
        .map(|(name, _, _)| name.as_str())
        .partition(|name| {
            only.iter().any(|named| named == name)
                || specs.iter().any(|spec| spec.name == *name && spec.pinned())
        });
    let unavailable = unavailable.len();

    if broken > 0 {
        bail!("{broken} documents did not round-trip through the model");
    }
    if !required.is_empty() {
        bail!(
            "{} could not be read, and a pinned document or one named with `--only` may not be \
             missing; run `cargo xtask corpus --fetch`",
            required.join(", ")
        );
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
    use std::time::Duration;

    use clap::Parser as _;
    use color_eyre::eyre::{self, OptionExt as _};

    use super::{
        Args, Outcome, Spec, fetch_all, require_committed, require_crate_packaging, select,
        transient, verdict,
    };

    /// A download is retried only when trying again may get another answer: a rate limit, a
    /// server error, no response, or a connection that dropped mid-transfer.
    #[test]
    fn a_download_is_retried_only_for_a_failure_in_transit() {
        use std::io::{Error, ErrorKind};
        let io = |kind: ErrorKind| ureq::Error::Io(Error::from(kind));
        let retried = [
            ureq::Error::StatusCode(429),
            ureq::Error::StatusCode(500),
            ureq::Error::StatusCode(599),
            ureq::Error::Timeout(ureq::Timeout::Global),
            ureq::Error::ConnectionFailed,
            io(ErrorKind::ConnectionReset),
            io(ErrorKind::UnexpectedEof),
            ureq::Error::Decompress("gzip", Error::from(ErrorKind::UnexpectedEof)),
        ];
        for error in &retried {
            assert!(transient(error), "{error} should be retried");
        }
        let final_at_once = [
            ureq::Error::StatusCode(404),
            ureq::Error::StatusCode(408),
            ureq::Error::StatusCode(499),
            ureq::Error::StatusCode(600),
            ureq::Error::HostNotFound,
            ureq::Error::BodyExceedsLimit(1),
            io(ErrorKind::PermissionDenied),
            ureq::Error::Decompress("gzip", Error::from(ErrorKind::InvalidData)),
        ];
        for error in &final_at_once {
            assert!(!transient(error), "{error} should fail at once");
        }
    }

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

    fn pinned(name: &str, url: &str, rev: &str) -> eyre::Result<Spec> {
        toml::from_str(&indoc::formatdoc! {r#"
            name = "{name}"
            url = "{url}"
            rev = "{rev}"
        "#})
        .map_err(Into::into)
    }

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A pinned document downloads from its commit, in place of the branch its url names.
    #[test_util::test]
    fn a_pinned_document_downloads_from_its_commit() {
        let spec = pinned(
            "oxide",
            "https://raw.githubusercontent.com/oxidecomputer/oxide.rs/main/oxide.json",
            REV,
        )?;
        assert_eq!(
            spec.download_url().ok_or_eyre("a pinned url")?,
            format!("https://raw.githubusercontent.com/oxidecomputer/oxide.rs/{REV}/oxide.json")
        );
    }

    /// A rev is a full commit on a GitHub raw url; anything else could not pin the bytes.
    #[test_util::test]
    fn a_rev_that_cannot_pin_its_document_is_refused() {
        let short = pinned(
            "a",
            "https://raw.githubusercontent.com/o/r/main/a.json",
            "0123abc",
        )?;
        assert!(super::lint_manifest(&[short]).is_err());
        let elsewhere = pinned("b", "https://example.invalid/b.json", REV)?;
        assert!(super::lint_manifest(&[elsewhere]).is_err());
    }

    /// `--pinned` keeps committed and pinned documents and leaves out live ones.
    #[test_util::test]
    fn pinned_runs_leave_out_live_documents() {
        let specs = vec![
            manifest(&["petstore-31"])?.remove(0),
            pinned(
                "oxide",
                "https://raw.githubusercontent.com/o/r/main/oxide.json",
                REV,
            )?,
            fetched("posthog")?,
        ];
        let selected = select(&specs, &parse(&["--pinned"]))?;
        let names: Vec<&str> = selected.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ["petstore-31", "oxide"]);
    }

    /// A committed document is checked against its snapshot without any download, and a pinned
    /// document with no snapshot to check against fails the fetch.
    #[test_util::test]
    fn pinned_documents_are_checked_against_their_snapshots() {
        let petstore: Spec = toml::from_str(indoc::indoc! {r#"
            name = "petstore-31"
            file = "petstore-31.yaml"
            local = true
        "#})?;
        fetch_all(&[petstore], false)?;

        let unrecorded = manifest(&["no-snapshot-records-this"])?;
        let error = fetch_all(&unrecorded, false)
            .err()
            .ok_or_eyre("a pinned document without a snapshot cannot be verified")?;
        assert!(error.to_string().contains("no snapshot"), "{error}");
    }

    /// A pinned document can always be fetched again, so it may never be the one missing.
    #[test_util::test]
    fn an_unreadable_pinned_document_fails_the_run() {
        let specs = vec![pinned(
            "oxide",
            "https://raw.githubusercontent.com/o/r/main/oxide.json",
            REV,
        )?];
        assert!(verdict(&specs, &[], &[unavailable("oxide")]).is_err());
    }

    fn unavailable(name: &str) -> (String, Outcome, Duration) {
        (
            name.to_owned(),
            Outcome::Unavailable(format!("{name}: not found")),
            Duration::ZERO,
        )
    }

    fn fetched(name: &str) -> eyre::Result<Spec> {
        toml::from_str(&indoc::formatdoc! {r#"
            name = "{name}"
            url = "https://example.invalid/{name}.json"
        "#})
        .map_err(Into::into)
    }

    /// One fetched document may be missing, because one corpus member is only ever fetched live
    /// and its endpoint is not progeny's to keep up; a second means the cache is not populated.
    #[test_util::test]
    fn one_unreadable_fetched_document_is_tolerated() {
        let specs = vec![fetched("launchdarkly")?, fetched("oxide")?];
        verdict(&specs, &[], &[unavailable("launchdarkly")])?;
        assert!(
            verdict(
                &specs,
                &[],
                &[unavailable("launchdarkly"), unavailable("oxide")]
            )
            .is_err()
        );
    }

    /// A run pointed at a document by name has checked nothing if that document is missing, so
    /// the tolerance for one unreadable fetched document does not reach it.
    #[test_util::test]
    fn a_named_document_that_cannot_be_read_fails_the_run() {
        let specs = vec![fetched("oxide")?];
        let error = verdict(&specs, &["oxide".to_owned()], &[unavailable("oxide")])
            .err()
            .ok_or_eyre("a document named with `--only` may not be missing")?;
        assert!(error.to_string().contains("oxide"), "{error}");
    }

    /// A committed document missing from the checkout is a broken tree, never the one tolerated
    /// download failure.
    #[test_util::test]
    fn a_committed_document_that_cannot_be_read_fails_the_run() {
        let specs = manifest(&["bare-operation"])?;
        assert!(verdict(&specs, &[], &[unavailable("bare-operation")]).is_err());

        // The same holds before any document is checked, for one the run did not even select.
        let present: Spec = toml::from_str(indoc::indoc! {r#"
            name = "petstore-31"
            file = "petstore-31.yaml"
            local = true
        "#})?;
        require_committed(std::slice::from_ref(&present))?;
        let mut specs = manifest(&["no-such-fixture"])?;
        specs.insert(0, present);
        let error = require_committed(&specs)
            .err()
            .ok_or_eyre("an unreadable committed document fails the run")?;
        assert!(
            format!("{error:#}").contains("no-such-fixture"),
            "{error:#}"
        );
    }

    /// Workspace packaging puts the client and server in crates of their own, so there is no
    /// feature to turn off and the matrix would check nothing it claims to.
    #[test_util::test]
    fn the_feature_matrix_is_refused_outside_crate_packaging() {
        let refused = parse(&["--compile", "--feature-matrix", "--packaging", "workspace"]);
        assert!(require_crate_packaging(&refused).is_err());
        require_crate_packaging(&parse(&["--compile", "--feature-matrix"]))?;
        require_crate_packaging(&parse(&[
            "--compile",
            "--feature-matrix",
            "--packaging",
            "crate",
        ]))?;
    }
}
