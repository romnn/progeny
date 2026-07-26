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

    /// Corpus documents to generate and measure.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Repetitions per variant.
    #[arg(long, default_value_t = 4)]
    reps: usize,

    /// Compiler jobs. Anything but 1 makes the numbers about the scheduler.
    #[arg(long, default_value_t = 1)]
    jobs: usize,

    /// Refuse to start a repetition while the one-minute load average is above this.
    #[arg(long, default_value_t = 1.0)]
    max_load: f64,

    /// A checked-in baseline to compare against or to write.
    #[arg(long, value_name = "PATH")]
    baseline: Option<Utf8PathBuf>,

    /// Fail when a measurement regressed past the threshold.
    #[arg(long, requires = "baseline")]
    check: bool,

    /// Overwrite the baseline with this run's measurements.
    #[arg(long, requires = "baseline", conflicts_with = "check")]
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

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BaselineEntry {
    cpu_seconds: f64,
    peak_rss_bytes: u64,
}

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

    let Some(crate_dir) = &args.crate_dir else {
        // The measurement engine is complete and exercised; what is missing is the thing to
        // measure. Say exactly that rather than pretending to run.
        bail!(
            "nothing to measure yet: generating a crate needs the renderers, which do not exist. \
             Pass --crate-dir <path> to measure a crate that already exists, which is how this \
             harness is exercised until then.{}",
            if args.specs.is_empty() {
                String::new()
            } else {
                format!(" (asked for: {})", args.specs.join(", "))
            }
        );
    };

    let package = package_name(crate_dir)?;
    println!(
        "bench-compile: {package} at {crate_dir}, {} reps",
        args.reps
    );
    println!(
        "  free memory before starting: {}",
        available_memory().map_or_else(|| "unknown".to_owned(), format_bytes)
    );

    // One variant for now. Variants become `Config` points — chiefly the serde strategy — once
    // there is more than one way to render a crate; the A-B-B-A ordering below is already written
    // for that and collapses to plain repetitions with a single variant.
    let variants = ["as-is"];
    let mut summaries: BTreeMap<&str, Summary> = BTreeMap::new();

    for rep in 0..args.reps {
        for variant in order(&variants, rep) {
            let sample = measure_once(crate_dir, &package, args)?;
            let summary = summaries.entry(variant).or_default();
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

    report(&package, &summaries);
    if let Some(path) = &args.baseline {
        return baseline(path, &package, &summaries, args);
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

fn measure_once(crate_dir: &Utf8PathBuf, package: &str, args: &Args) -> Result<Sample> {
    wait_for_quiet(args.max_load)?;

    // A cached crate measures nothing, so discard just this package's artifacts and leave its
    // dependencies compiled: what is under test is the generated code, not the ecosystem.
    let cleaned = Command::new("cargo")
        .args(["clean", "--quiet", "-p", package, "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .status()
        .context("running cargo clean")?;
    if !cleaned.success() {
        bail!("cargo clean failed for {package}");
    }

    let available_before = available_memory();
    let load_before = load_average()?;

    let runner = std::env::current_exe().context("locating this executable")?;
    // `--measure` takes the rest of the line, so the command follows it directly; a `--`
    // separator would be swallowed as its first value.
    let output = Command::new(runner)
        .arg("bench-compile")
        .arg("--measure")
        .arg("cargo")
        .arg("check")
        .arg("--quiet")
        .arg("--lib")
        .arg("--jobs")
        .arg(args.jobs.to_string())
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
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
        bail!("`cargo check` failed for {package}; there is nothing to measure");
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

fn report(package: &str, summaries: &BTreeMap<&str, Summary>) {
    println!();
    println!("{package}");
    for (variant, summary) in summaries {
        match (summary.mean_cpu(), summary.mean_rss()) {
            (Some(cpu), Some(rss)) => {
                println!(
                    "  {variant:<28} {cpu:>7.2} s cpu   {:>10} peak rss   ({} kept, {} discarded)",
                    format_bytes(rss),
                    summary.kept.len(),
                    summary.discarded
                );
                if summary.pressured > 0 {
                    println!(
                        "    {} of those repetitions ran with thin memory; treat the peak as a \
                         floor",
                        summary.pressured
                    );
                }
            }
            _ => println!(
                "  {variant:<28} no usable repetitions ({} discarded)",
                summary.discarded
            ),
        }
    }
}

fn baseline(
    path: &Utf8PathBuf,
    package: &str,
    summaries: &BTreeMap<&str, Summary>,
    args: &Args,
) -> Result<()> {
    let mut entries: BTreeMap<String, BaselineEntry> = if path.exists() {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        toml::from_str(&text).with_context(|| format!("parsing {path}"))?
    } else {
        BTreeMap::new()
    };

    let mut regressions = Vec::new();
    for (variant, summary) in summaries {
        let Some((cpu, rss)) = summary.mean_cpu().zip(summary.mean_rss()) else {
            continue;
        };
        let key = format!("{package}.{variant}");
        if args.write_baseline {
            entries.insert(
                key,
                BaselineEntry {
                    cpu_seconds: cpu,
                    peak_rss_bytes: rss,
                },
            );
            continue;
        }
        let Some(previous) = entries.get(&key) else {
            println!("  {key}: no baseline yet");
            continue;
        };
        let cpu_delta = percent(previous.cpu_seconds, cpu);
        let rss_delta = percent(as_f64(previous.peak_rss_bytes), as_f64(rss));
        println!("  {key}: cpu {cpu_delta:+.1}%, peak rss {rss_delta:+.1}%");
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

fn wait_for_quiet(max_load: f64) -> Result<()> {
    const ATTEMPTS: usize = 30;
    for attempt in 0..ATTEMPTS {
        let load = load_average()?;
        if load <= max_load {
            return Ok(());
        }
        if attempt == 0 {
            println!("  waiting for the machine to go quiet (load {load:.2} > {max_load:.2})");
        }
        std::thread::sleep(Duration::from_secs(10));
    }
    bail!("the machine did not go quiet; timing on a busy machine measures the machine")
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
