//! What a generated crate costs to compile.
//!
//! Compile cost is a product property here, not an afterthought, so the harness exists before
//! there is anything to benchmark: the measurement discipline is the hard part, and it is the part
//! that has to be right before any number it produces is worth quoting.
//!
//! The discipline, and why each rule is there:
//!
//! * **A-B-B-A ordering** between variants, so a drift in machine state over the run cannot be
//!   mistaken for a difference between variants.
//! * **`--jobs 1`**, because parallel rustc invocations make both CPU time and peak resident set
//!   size depend on scheduling rather than on the code.
//! * **Load gating**, and *discard* any repetition whose load rose while it ran rather than
//!   averaging it in. A crowded baseline repetition silently inflates the apparent win — that is
//!   not a hypothetical, it happened, by three points.
//! * **Peak RSS taken with little free memory is an underestimate, never a win**: the kernel
//!   reclaims under pressure and the number reads low. Such repetitions are reported and refused
//!   as evidence of an improvement.
//! * **One fresh measuring process per repetition.** `getrusage(RUSAGE_CHILDREN)` accumulates over
//!   the lifetime of the process asking, so a long-lived runner would carry the largest earlier
//!   repetition into every later reading.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use clap::Args as ClapArgs;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Measure this crate directory as it stands, rather than generating one.
    #[arg(long, value_name = "PATH")]
    crate_dir: Option<Utf8PathBuf>,

    /// Corpus documents to generate and measure. Defaults to the quick tier.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Measure both serde strategies rather than only the derive.
    ///
    /// This is the A/B the hand-written path exists for: same document, same configuration, one
    /// flag apart.
    #[arg(long)]
    ab: bool,

    /// Render what would be measured and stop, without compiling or timing anything.
    ///
    /// The measurement needs a quiet machine; rendering does not. Separating them is what lets the
    /// "A" side of a comparison be captured at the moment it is still true, and measured later.
    #[arg(long)]
    generate_only: bool,

    /// Measure what an earlier run rendered, rather than rendering again.
    ///
    /// The other half of `--generate-only`, and the reason both exist: a figure about *this* tree
    /// has to be taken from crates rendered by *this* tree, and the machine may not be quiet for
    /// hours. Reusing pins the subject; only the measurement waits.
    #[arg(long, conflicts_with_all = ["generate_only", "crate_dir"])]
    reuse: bool,

    /// Repetitions per variant.
    #[arg(long, default_value_t = 4)]
    reps: usize,

    /// Compiler jobs. Anything but 1 makes the numbers about the scheduler.
    #[arg(long, default_value_t = 1)]
    jobs: usize,

    /// Refuse to start a repetition while the one-minute load average is above this.
    #[arg(long, default_value_t = 1.0)]
    max_load: f64,

    /// Minutes to wait for the machine to go quiet before giving up on a repetition.
    ///
    /// Five is right for a run somebody is watching. A take that has been left to find its own
    /// window wants hours, and the alternative — measuring anyway — is the thing the whole harness
    /// exists to refuse.
    #[arg(long, default_value_t = 5)]
    max_wait: u64,

    /// The checked-in baseline to compare against or to write.
    #[arg(long, value_name = "PATH")]
    baseline: Option<Utf8PathBuf>,

    /// Fail when a measurement regressed past the threshold.
    #[arg(long)]
    check: bool,

    /// Overwrite the baseline with this run's measurements.
    #[arg(long, conflicts_with = "check")]
    write_baseline: bool,

    /// How much worse than the baseline counts as a regression, in percent.
    #[arg(long, default_value_t = 10.0)]
    threshold: f64,

    /// Internal: run one command in this fresh process and report what it cost.
    #[arg(
        long,
        num_args = 1..,
        value_name = "COMMAND",
        allow_hyphen_values = true,
        hide = true
    )]
    measure: Vec<String>,
}

/// What one repetition cost.
#[derive(Debug, Clone, Copy)]
struct Sample {
    cpu_seconds: f64,
    peak_rss_bytes: u64,
    load_before: f64,
    load_after: f64,
    /// Whether free memory was thin enough that the peak is an underestimate.
    pressured: bool,
}

impl Sample {
    /// Whether the machine got busier while this repetition ran.
    ///
    /// The tolerance is deliberately small: a run that shared the machine with something else did
    /// not measure the code.
    fn crowded(&self) -> bool {
        self.load_after > self.load_before + 0.25
    }
}

/// The measurements for one variant of one crate.
#[derive(Debug, Default)]
struct Summary {
    kept: Vec<Sample>,
    discarded: usize,
    pressured: usize,
}

impl Summary {
    fn mean_cpu(&self) -> Option<f64> {
        let count = u32::try_from(self.kept.len()).ok()?;
        if count == 0 {
            return None;
        }
        let total: f64 = self.kept.iter().map(|sample| sample.cpu_seconds).sum();
        Some(total / f64::from(count))
    }

    fn mean_rss(&self) -> Option<u64> {
        let count = u64::try_from(self.kept.len()).ok()?;
        if count == 0 {
            return None;
        }
        let total: u64 = self.kept.iter().map(|sample| sample.peak_rss_bytes).sum();
        Some(total / count)
    }
}

/// One recorded measurement, and the conditions it was taken under.
///
/// The conditions are part of the record rather than a note somewhere, because a baseline's whole
/// job is to be compared against later — and a comparison between a number taken on a busy machine
/// and one taken on an idle machine is not a comparison. The rule this encodes: a baseline may be
/// written on a shared machine, but it may never be *silently* written on one.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BaselineEntry {
    cpu_seconds: f64,
    peak_rss_bytes: u64,
    /// Repetitions kept, and repetitions thrown away because the machine got busier during them.
    kept: usize,
    discarded: usize,
    /// The one-minute load average when the run started, and the cores it was spread over.
    load: f64,
    cores: usize,
    /// Set when free memory was thin enough that the peak is a floor rather than a result.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pressured: bool,
}

/// One crate to measure, and which rendering of its document it is.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct Target {
    /// The document this was generated from, or the directory it was found in.
    subject: String,
    variant: String,
    package: String,
    directory: Utf8PathBuf,
}

/// What one run rendered, and which tree it rendered from.
///
/// Written beside the crates so that a measurement taken days later still says what it measured.
/// A benchmark whose subject cannot be identified afterwards is a number, not evidence.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Rendering {
    /// The commit the generator was at, and whether the worktree had uncommitted changes.
    revision: String,
    dirty: bool,
    targets: Vec<Target>,
}

/// Where the rendering plan lives, so `--reuse` needs no guesses about directory names.
fn rendering_path() -> Utf8PathBuf {
    crate::generated::scratch_root().join("bench-rendering.toml")
}

/// The serde strategy each variant renders with.
const DERIVE: &str = "derive";
const HAND_WRITTEN: &str = "hand-written";

pub fn run(args: &Args) -> Result<()> {
    if !args.measure.is_empty() {
        return measure_child(&args.measure);
    }
    if args.jobs != 1 {
        println!(
            "warning: --jobs {} makes both numbers depend on scheduling; the established \
             methodology is --jobs 1",
            args.jobs
        );
    }
    crate::generated::require_cargo()?;

    let subjects = plan(args)?;
    if args.generate_only {
        for (_, targets) in &subjects {
            for target in targets {
                println!("  {} → {}", key_of(target), target.directory);
            }
        }
        println!();
        println!(
            "rendered but not measured. `--reuse` measures exactly these crates, however far the \
             generator moves in the meantime"
        );
        return Ok(());
    }
    println!(
        "  free memory before starting: {}",
        available_memory().map_or_else(|| "unknown".to_owned(), format_bytes)
    );

    let mut summaries: BTreeMap<String, Summary> = BTreeMap::new();
    for (subject, targets) in &subjects {
        let variants: Vec<&str> = targets
            .iter()
            .map(|target| target.variant.as_str())
            .collect();
        println!();
        println!("{subject}: {} reps × {}", args.reps, variants.join(", "));
        // Dependencies are compiled once, outside the measurement, so the first repetition
        // measures the generated crate rather than the ecosystem underneath it.
        for target in targets {
            warm_up(target)?;
        }
        for rep in 0..args.reps {
            for variant in order(&variants, rep) {
                let Some(target) = targets.iter().find(|target| target.variant == variant) else {
                    continue;
                };
                let sample = measure_once(target, args)?;
                let summary = summaries.entry(key_of(target)).or_default();
                if sample.crowded() {
                    summary.discarded += 1;
                    println!(
                        "  rep {rep} {variant}: discarded, load rose {:.2} → {:.2}",
                        sample.load_before, sample.load_after
                    );
                    continue;
                }
                if sample.pressured {
                    summary.pressured += 1;
                }
                println!(
                    "  rep {rep} {variant}: {:.2} s cpu, {} peak rss{}",
                    sample.cpu_seconds,
                    format_bytes(sample.peak_rss_bytes),
                    if sample.pressured {
                        " (memory was thin: the peak is a floor, not a result)"
                    } else {
                        ""
                    }
                );
                summary.kept.push(sample);
            }
        }
    }

    report(&summaries);
    compare(&subjects, &summaries);
    // Writing or checking a baseline without naming one means the checked-in one: the point of a
    // baseline is that everybody compares against the same file.
    let path = args.baseline.clone().or_else(|| {
        (args.write_baseline || args.check)
            .then(|| crate::paths::corpus_root().join("baseline.toml"))
    });
    if let Some(path) = path {
        return baseline(&path, &summaries, args);
    }
    Ok(())
}

/// The baseline key: the document, then the rendering. Stable across runs by construction.
fn key_of(target: &Target) -> String {
    format!("{}.{}", target.subject, target.variant)
}

/// What this run will measure, generated and written out, grouped by document.
///
/// Generation happens up front rather than between repetitions: A-B-B-A ordering only means
/// anything if both variants are sitting on disk before the first measurement starts.
fn plan(args: &Args) -> Result<Vec<(String, Vec<Target>)>> {
    if args.reuse {
        return reuse(args);
    }
    if let Some(crate_dir) = &args.crate_dir {
        let package = package_name(crate_dir)?;
        println!(
            "bench-compile: {package} at {crate_dir}, {} reps",
            args.reps
        );
        return Ok(vec![(
            package.clone(),
            vec![Target {
                subject: package.clone(),
                variant: "as-is".to_owned(),
                package,
                directory: crate_dir.clone(),
            }],
        )]);
    }

    let specs = crate::corpus::load_manifest()?;
    let wanted = if args.specs.is_empty() {
        crate::corpus::quick_tier()?
    } else {
        args.specs.clone()
    };
    let variants: &[&'static str] = if args.ab {
        &[DERIVE, HAND_WRITTEN]
    } else {
        &[DERIVE]
    };
    println!(
        "bench-compile: {} documents × {} × {} reps, generated into {}",
        wanted.len(),
        variants.join(" and "),
        args.reps,
        crate::generated::scratch_root()
    );

    let mut planned = Vec::new();
    for name in &wanted {
        let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
            bail!("no corpus document named `{name}`");
        };
        let path = crate::corpus::document_path(spec);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading {path}; run `cargo xtask corpus --fetch`"))?;

        let mut targets = Vec::new();
        for &variant in variants {
            let mut config = crate::corpus::config_for(spec);
            config.serde_impl = if variant == HAND_WRITTEN {
                progeny::SerdeImpl::HandWrittenWhereEligible
            } else {
                progeny::SerdeImpl::DeriveAlways
            };
            let output = progeny::generate(&bytes, &config)
                .with_context(|| format!("generating {name} ({variant})"))?;
            let directory = crate::generated::write(&format!("bench-{variant}-{name}"), &output)?;
            targets.push(Target {
                subject: name.clone(),
                variant: variant.to_owned(),
                package: config.package.name.clone(),
                directory,
            });
        }
        planned.push((name.clone(), targets));
    }
    record(&planned)?;
    Ok(planned)
}

/// Write down what was rendered and from which tree.
fn record(planned: &[(String, Vec<Target>)]) -> Result<()> {
    let (revision, dirty) = revision();
    let rendering = Rendering {
        revision,
        dirty,
        targets: planned
            .iter()
            .flat_map(|(_, targets)| targets.iter().cloned())
            .collect(),
    };
    let path = rendering_path();
    let text = toml::to_string_pretty(&rendering).context("rendering the bench plan")?;
    std::fs::write(&path, text).with_context(|| format!("writing {path}"))
}

/// Measure the crates an earlier run rendered, without rendering anything.
fn reuse(args: &Args) -> Result<Vec<(String, Vec<Target>)>> {
    let path = rendering_path();
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!("reading {path}; --reuse measures what --generate-only rendered, and nothing has")
    })?;
    let rendering: Rendering = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;

    let (here, _) = revision();
    println!(
        "bench-compile: reusing {} crates rendered from {}{}, {} reps",
        rendering.targets.len(),
        rendering.revision,
        if rendering.dirty {
            " with uncommitted changes"
        } else {
            ""
        },
        args.reps
    );
    // Said rather than refused: measuring an older rendering is the *point* of `--reuse`, and the
    // only thing that would make it a mistake is not knowing.
    if here != rendering.revision {
        println!(
            "  note: the generator is at {here} now, so this measures the earlier tree and not \
             this one"
        );
    }

    let mut planned: Vec<(String, Vec<Target>)> = Vec::new();
    for target in rendering.targets {
        let manifest = target.directory.join("Cargo.toml");
        if !manifest.exists() {
            bail!(
                "{} is gone, so there is nothing to reuse; re-run with --generate-only",
                target.directory
            );
        }
        if !args.specs.is_empty() && !args.specs.contains(&target.subject) {
            continue;
        }
        match planned.iter_mut().find(|(name, _)| name == &target.subject) {
            Some((_, targets)) => targets.push(target),
            None => planned.push((target.subject.clone(), vec![target])),
        }
    }
    if planned.is_empty() {
        bail!("the recorded rendering has nothing matching the documents asked for");
    }
    Ok(planned)
}

/// The commit the generator is at, and whether the worktree has uncommitted changes.
fn revision() -> (String, bool) {
    let git = |arguments: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .current_dir(crate::paths::workspace_root())
            .args(arguments)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let head =
        git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "an unknown tree".to_owned());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    (head, dirty)
}

/// Compile the crate once without measuring, so its dependencies are built and cached.
fn warm_up(target: &Target) -> Result<()> {
    let status = Command::new("cargo")
        .current_dir(&target.directory)
        .env("CARGO_TARGET_DIR", crate::generated::shared_target())
        .env_remove("RUSTFLAGS")
        .args(["check", "--quiet", "--lib"])
        .status()
        .with_context(|| format!("warming up {}", target.directory))?;
    if !status.success() {
        bail!(
            "`cargo check` failed for {} ({}); there is nothing to measure",
            target.subject,
            target.variant
        );
    }
    Ok(())
}

/// A-B-B-A: the second half of each pair of repetitions runs the variants in reverse.
fn order<'a>(variants: &[&'a str], rep: usize) -> Vec<&'a str> {
    let mut ordered = variants.to_vec();
    if rep % 2 == 1 {
        ordered.reverse();
    }
    ordered
}

fn measure_once(target: &Target, args: &Args) -> Result<Sample> {
    wait_for_quiet(args.max_load, args.max_wait)?;

    // A cached crate measures nothing, so discard just this package's artifacts and leave its
    // dependencies compiled: what is under test is the generated code, not the ecosystem.
    let cleaned = Command::new("cargo")
        .env("CARGO_TARGET_DIR", crate::generated::shared_target())
        .args(["clean", "--quiet", "-p", &target.package, "--manifest-path"])
        .arg(target.directory.join("Cargo.toml"))
        .status()
        .context("running cargo clean")?;
    if !cleaned.success() {
        bail!("cargo clean failed for {}", target.package);
    }

    let available_before = available_memory();
    let load_before = load_average()?;

    let runner = std::env::current_exe().context("locating this executable")?;
    // `--measure` takes the rest of the line, so the command follows it directly; a `--`
    // separator would be swallowed as its first value.
    let output = Command::new(runner)
        .env("CARGO_TARGET_DIR", crate::generated::shared_target())
        .env_remove("RUSTFLAGS")
        .arg("bench-compile")
        .arg("--measure")
        .arg("cargo")
        .arg("check")
        .arg("--quiet")
        .arg("--lib")
        .arg("--jobs")
        .arg(args.jobs.to_string())
        .arg("--manifest-path")
        .arg(target.directory.join("Cargo.toml"))
        .output()
        .context("running the measuring process")?;
    if !output.status.success() {
        bail!(
            "the measuring process failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let reported = String::from_utf8_lossy(&output.stdout);
    let reported: MeasuredCost = serde_json::from_str(reported.trim())
        .with_context(|| format!("reading the measurement: {reported}"))?;
    if !reported.ok {
        bail!(
            "`cargo check` failed for {}; there is nothing to measure",
            target.package
        );
    }

    let load_after = load_average()?;
    // Peak RSS is only meaningful with headroom: under pressure the kernel reclaims and the
    // high-water mark reads low, so a "win" measured this way is an artefact.
    let pressured =
        available_before.is_some_and(|available| available < reported.peak_rss_bytes * 2);

    Ok(Sample {
        cpu_seconds: reported.cpu_seconds,
        peak_rss_bytes: reported.peak_rss_bytes,
        load_before,
        load_after,
        pressured,
    })
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct MeasuredCost {
    cpu_seconds: f64,
    peak_rss_bytes: u64,
    ok: bool,
}

fn report(summaries: &BTreeMap<String, Summary>) {
    println!();
    println!("measurements");
    for (key, summary) in summaries {
        match (summary.mean_cpu(), summary.mean_rss()) {
            (Some(cpu), Some(rss)) => {
                println!(
                    "  {key:<34} {cpu:>7.2} s cpu   {:>10} peak rss   ({} kept, {} discarded)",
                    format_bytes(rss),
                    summary.kept.len(),
                    summary.discarded
                );
                if summary.pressured > 0 {
                    println!(
                        "    {} of those repetitions ran with thin memory; treat the peak as a \
                         floor, not a result",
                        summary.pressured
                    );
                }
            }
            _ => println!(
                "  {key:<34} no usable repetitions ({} discarded)",
                summary.discarded
            ),
        }
    }
}

/// The A/B, where a document was measured both ways.
///
/// Reported separately from the baseline comparison because they answer different questions: the
/// baseline asks "did this change since last time", and this asks "is the hand-written path worth
/// having". A repetition taken under memory pressure disqualifies the RSS half of the answer
/// outright — the kernel reclaims under pressure and the number reads low, which is an artefact
/// pointing the same way as a win.
fn compare(subjects: &[(String, Vec<Target>)], summaries: &BTreeMap<String, Summary>) {
    let paired: Vec<&(String, Vec<Target>)> = subjects
        .iter()
        .filter(|(_, targets)| targets.len() > 1)
        .collect();
    if paired.is_empty() {
        return;
    }
    println!();
    println!("hand-written against derive");
    for (subject, _) in paired {
        let (Some(before), Some(after)) = (
            summaries.get(&format!("{subject}.{DERIVE}")),
            summaries.get(&format!("{subject}.{HAND_WRITTEN}")),
        ) else {
            continue;
        };
        let (Some(cpu_before), Some(cpu_after)) = (before.mean_cpu(), after.mean_cpu()) else {
            println!("  {subject:<24} no usable repetitions");
            continue;
        };
        let (Some(rss_before), Some(rss_after)) = (before.mean_rss(), after.mean_rss()) else {
            continue;
        };
        let pressured = before.pressured > 0 || after.pressured > 0;
        println!(
            "  {subject:<24} cpu {:+.1}%   peak rss {:+.1}%{}",
            percent(cpu_before, cpu_after),
            percent(as_f64(rss_before), as_f64(rss_after)),
            if pressured {
                "   (memory was thin; the rss figure is not evidence)"
            } else {
                ""
            }
        );
    }
}

fn baseline(path: &Utf8PathBuf, summaries: &BTreeMap<String, Summary>, args: &Args) -> Result<()> {
    let mut entries: BTreeMap<String, BaselineEntry> = if path.exists() {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        toml::from_str(&text).with_context(|| format!("parsing {path}"))?
    } else {
        BTreeMap::new()
    };

    let mut regressions = Vec::new();
    println!();
    for (key, summary) in summaries {
        let Some((cpu, rss)) = summary.mean_cpu().zip(summary.mean_rss()) else {
            continue;
        };
        if args.write_baseline {
            entries.insert(
                key.clone(),
                BaselineEntry {
                    cpu_seconds: cpu,
                    peak_rss_bytes: rss,
                    kept: summary.kept.len(),
                    discarded: summary.discarded,
                    load: summary
                        .kept
                        .first()
                        .map_or(0.0, |sample| sample.load_before),
                    cores: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
                    pressured: summary.pressured > 0,
                },
            );
            continue;
        }
        let Some(previous) = entries.get(key) else {
            println!("  {key}: no baseline yet");
            continue;
        };
        let cpu_delta = percent(previous.cpu_seconds, cpu);
        let rss_delta = percent(as_f64(previous.peak_rss_bytes), as_f64(rss));
        println!("  {key}: cpu {cpu_delta:+.1}%, peak rss {rss_delta:+.1}%");
        // Said every time rather than only on a regression: a reader comparing two numbers is
        // entitled to know whether they were taken under comparable conditions, and finding that
        // out afterwards is what makes a bad comparison expensive.
        let here = summary
            .kept
            .first()
            .map_or(0.0, |sample| sample.load_before);
        if (previous.load - here).abs() > 1.0 {
            println!(
                "    conditions differ: the baseline was taken at load {:.2} over {} cores and \
                 this run at load {here:.2}; a difference of a few points may be the machine",
                previous.load, previous.cores
            );
        }
        if previous.pressured || summary.pressured > 0 {
            println!(
                "    one side ran with thin memory, so the peak rss comparison is not evidence"
            );
        }
        if args.check {
            if cpu_delta > args.threshold {
                regressions.push(format!("{key}: cpu {cpu_delta:+.1}%"));
            }
            if rss_delta > args.threshold {
                regressions.push(format!("{key}: peak rss {rss_delta:+.1}%"));
            }
        }
    }

    if args.write_baseline {
        let text = toml::to_string_pretty(&entries).context("rendering the baseline")?;
        std::fs::write(path, text).with_context(|| format!("writing {path}"))?;
        println!("baseline written to {path}");
    }
    if !regressions.is_empty() {
        for regression in &regressions {
            println!("  regression: {regression}");
        }
        bail!(
            "{} measurements regressed past {}%",
            regressions.len(),
            args.threshold
        );
    }
    Ok(())
}

fn percent(before: f64, after: f64) -> f64 {
    if before == 0.0 {
        return 0.0;
    }
    (after - before) / before * 100.0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "byte counts and their ratios; a difference beyond 2^53 bytes is not a difference \
              anyone is measuring"
)]
fn as_f64(value: u64) -> f64 {
    value as f64
}

fn package_name(crate_dir: &Utf8PathBuf) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        package: Package,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
    }

    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let manifest: Manifest = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
    Ok(manifest.package.name)
}

fn wait_for_quiet(max_load: f64, max_wait: u64) -> Result<()> {
    const INTERVAL: u64 = 10;
    let attempts = (max_wait * 60 / INTERVAL).max(1);
    for attempt in 0..attempts {
        let load = load_average()?;
        if load <= max_load {
            return Ok(());
        }
        // Every attempt would be noise and only the first would be silence: a wait measured in
        // hours should say it is still waiting, and roughly every ten minutes is enough for that.
        if attempt == 0 || attempt % 60 == 0 {
            println!("  waiting for the machine to go quiet (load {load:.2} > {max_load:.2})");
        }
        std::thread::sleep(Duration::from_secs(INTERVAL));
    }
    bail!(
        "the machine did not go quiet within {max_wait} minutes; timing on a busy machine measures \
         the machine"
    )
}

/// The one-minute load average.
fn load_average() -> Result<f64> {
    let text = std::fs::read_to_string("/proc/loadavg")
        .context("reading /proc/loadavg; load gating needs it")?;
    let first = text
        .split_whitespace()
        .next()
        .context("/proc/loadavg was empty")?;
    first
        .parse()
        .with_context(|| format!("reading a load average from {first:?}"))
}

/// Memory the kernel believes is available, in bytes.
fn available_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kibibytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kibibytes * 1024)
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", as_f64(bytes) / as_f64(GIB))
    } else {
        format!("{:.1} MiB", as_f64(bytes) / as_f64(MIB))
    }
}

/// Run one command and report what it and its descendants cost.
///
/// This runs in its own process on purpose: `getrusage(RUSAGE_CHILDREN)` reports a high-water mark
/// accumulated over the caller's whole life, so a runner that measured several repetitions itself
/// would report the largest earlier one forever.
#[cfg(unix)]
fn measure_child(command: &[String]) -> Result<()> {
    use nix::sys::resource::{UsageWho, getrusage};

    // Tolerate a `--` separator so the flag can also be driven by hand.
    let command = match command.split_first() {
        Some((first, rest)) if first == "--" => rest,
        _ => command,
    };
    let (program, arguments) = command
        .split_first()
        .context("--measure needs a command to run")?;

    let status = Command::new(program)
        .args(arguments)
        .status()
        .with_context(|| format!("running {program}"))?;

    let usage = getrusage(UsageWho::RUSAGE_CHILDREN).context("getrusage(RUSAGE_CHILDREN)")?;
    let user = duration_of(usage.user_time());
    let system = duration_of(usage.system_time());
    // `ru_maxrss` is in kibibytes on Linux and in bytes on macOS.
    let peak_kib = u64::try_from(usage.max_rss()).unwrap_or(0);
    let peak_rss_bytes = if cfg!(target_os = "macos") {
        peak_kib
    } else {
        peak_kib * 1024
    };

    let measured = MeasuredCost {
        cpu_seconds: user + system,
        peak_rss_bytes,
        ok: status.success(),
    };
    println!(
        "{}",
        serde_json::to_string(&measured).context("rendering the measurement")?
    );
    Ok(())
}

#[cfg(unix)]
fn duration_of(time: nix::sys::time::TimeVal) -> f64 {
    use nix::sys::time::TimeValLike;
    #[expect(
        clippy::cast_precision_loss,
        reason = "microseconds of CPU time; the loss begins after 285 years"
    )]
    let micros = time.num_microseconds() as f64;
    micros / 1_000_000.0
}

#[cfg(not(unix))]
fn measure_child(command: &[String]) -> Result<()> {
    let _ = command;
    bail!(
        "measuring CPU time and peak resident set size needs getrusage, which this platform does \
         not provide; run the benchmark on Linux or macOS"
    )
}

#[cfg(test)]
mod tests {
    use super::{Sample, Summary, format_bytes, order, percent};

    fn sample(cpu: f64, rss: u64, load_before: f64, load_after: f64) -> Sample {
        Sample {
            cpu_seconds: cpu,
            peak_rss_bytes: rss,
            load_before,
            load_after,
            pressured: false,
        }
    }

    #[test]
    fn a_rendering_survives_the_file_it_is_written_to() {
        // `--generate-only` writes this and `--reuse` reads it, possibly days apart and across a
        // rebuild of the tool. A shape that serializes and does not deserialize would strand the
        // one artifact the separation exists to preserve.
        let rendering = super::Rendering {
            revision: "013655a".to_owned(),
            dirty: true,
            targets: vec![super::Target {
                subject: "okta".to_owned(),
                variant: super::DERIVE.to_owned(),
                package: "corpus-okta".to_owned(),
                directory: camino::Utf8PathBuf::from("/tmp/bench-derive-okta"),
            }],
        };
        let text = toml::to_string_pretty(&rendering).unwrap();
        let read: super::Rendering = toml::from_str(&text).unwrap();
        assert_eq!(read.revision, "013655a");
        assert!(read.dirty);
        assert_eq!(read.targets.len(), 1);
        assert_eq!(super::key_of(&read.targets[0]), "okta.derive");
        assert_eq!(read.targets[0].directory, "/tmp/bench-derive-okta");
    }

    #[test]
    fn two_variants_run_abba() {
        let variants = ["a", "b"];
        assert_eq!(order(&variants, 0), ["a", "b"]);
        assert_eq!(order(&variants, 1), ["b", "a"]);
        assert_eq!(order(&variants, 2), ["a", "b"]);
        assert_eq!(order(&variants, 3), ["b", "a"]);
    }

    #[test]
    fn one_variant_is_just_repetitions() {
        assert_eq!(order(&["only"], 0), ["only"]);
        assert_eq!(order(&["only"], 1), ["only"]);
    }

    #[test]
    fn a_repetition_whose_load_rose_is_crowded() {
        assert!(!sample(1.0, 1, 0.10, 0.20).crowded());
        assert!(sample(1.0, 1, 0.10, 0.90).crowded());
        // Load falling is fine: the machine got quieter, not busier.
        assert!(!sample(1.0, 1, 0.90, 0.10).crowded());
    }

    #[test]
    fn a_summary_averages_only_what_it_kept() {
        let mut summary = Summary::default();
        assert_eq!(summary.mean_cpu(), None);
        summary.kept.push(sample(10.0, 1_000, 0.0, 0.0));
        summary.kept.push(sample(20.0, 3_000, 0.0, 0.0));
        summary.discarded = 7;
        assert_eq!(summary.mean_cpu(), Some(15.0));
        assert_eq!(summary.mean_rss(), Some(2_000));
    }

    #[test]
    fn a_change_is_reported_relative_to_where_it_started() {
        assert!((percent(100.0, 63.0) - -37.0).abs() < 1e-9);
        assert!((percent(100.0, 110.0) - 10.0).abs() < 1e-9);
        assert!((percent(0.0, 5.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 / 2), "1.5 MiB");
    }
}
