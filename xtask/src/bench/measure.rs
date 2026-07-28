//! One repetition: run it in a fresh process, read what it cost, and judge whether it counts.
//!
//! The judging lives beside the measuring because both are statements about a single repetition —
//! whether the machine was quiet enough to start, whether the repetition got the processor once it
//! did, and whether memory was thin enough to make the peak a floor. What the *recorded* numbers
//! have to satisfy before they may be quoted is [`super::baseline`]'s question.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::Args;
use super::plan::Target;

/// What one repetition cost.
#[derive(Debug, Clone, Copy)]
pub(super) struct Sample {
    pub(super) cpu_seconds: f64,
    pub(super) peak_rss_bytes: u64,
    pub(super) load_before: f64,
    pub(super) load_after: f64,
    /// Wall-clock seconds the repetition ran for, which is what decides how much of the load
    /// average it is answerable for itself.
    pub(super) wall_seconds: f64,
    /// Whether free memory was thin enough that the peak is an underestimate.
    pub(super) pressured: bool,
}

/// How much of its wall-clock time a repetition has to spend on a processor before it counts.
///
/// Deliberately loose. This is not a second load ceiling — [`super::baseline::discipline::MAX_LOAD`]
/// already refuses to *start* a repetition on a busy machine, and this catches only the machine
/// getting busy afterwards, which is the part the ceiling cannot see. Half means a repetition may
/// spend as much time waiting as running before anyone doubts it.
const MIN_PROGRESS: f64 = 0.5;

impl Sample {
    /// What fraction of a processor this repetition actually got.
    ///
    /// Above 1.0 whenever `rustc` threads inside its single job, which is common and fine: the
    /// number is only ever compared against a floor.
    pub(super) fn progress(&self) -> f64 {
        if self.wall_seconds <= 0.0 {
            return f64::INFINITY;
        }
        self.cpu_seconds / self.wall_seconds
    }

    /// Whether something else was competing for the processor while this repetition ran.
    ///
    /// Asked **of the repetition, not of the machine**, and that is the whole correction. The rule
    /// used to compare the one-minute load average before and after against a flat 0.25 — but a
    /// repetition *is itself load*. That average is an exponentially weighted mean over sixty
    /// seconds, so a 21-second measurement raises it by about 0.3 unaided, and `okta` duly
    /// discarded four of six `derive` repetitions and **none** of its `hand-written` ones, the only
    /// difference between them being 21 seconds against 7. A filter that discards the slow
    /// variant's repetitions *because it is slow* biases the exact comparison this harness exists
    /// to make, in the direction of a larger apparent win.
    ///
    /// Starvation is visible from inside the measurement, so it needs no model of the load
    /// average's decay, nobody's thread count, and no guess about what else is running: when the
    /// processor is contended, wall-clock time runs away from CPU time. Measured, local, and true
    /// whatever the rest of the machine is doing.
    ///
    /// What this deliberately does *not* catch is memory-bandwidth contention, which inflates CPU
    /// seconds and wall seconds together and so leaves the ratio alone. That is what the load
    /// ceiling is for, and why both exist.
    pub(super) fn crowded(&self) -> bool {
        self.progress() < MIN_PROGRESS
    }
}

/// The measurements for one variant of one crate.
#[derive(Debug, Default)]
pub(super) struct Summary {
    pub(super) kept: Vec<Sample>,
    pub(super) discarded: usize,
    pub(super) pressured: usize,
}

impl Summary {
    pub(super) fn mean_cpu(&self) -> Option<f64> {
        let count = u32::try_from(self.kept.len()).ok()?;
        if count == 0 {
            return None;
        }
        let total: f64 = self.kept.iter().map(|sample| sample.cpu_seconds).sum();
        Some(total / f64::from(count))
    }

    pub(super) fn mean_rss(&self) -> Option<u64> {
        let count = u64::try_from(self.kept.len()).ok()?;
        if count == 0 {
            return None;
        }
        let total: u64 = self.kept.iter().map(|sample| sample.peak_rss_bytes).sum();
        Some(total / count)
    }

    /// The busiest a kept repetition started at.
    ///
    /// The worst rather than the first, which is what was recorded before: taking the first
    /// flatters a run whose *later* repetitions were the crowded ones, and it is the worst
    /// repetition that decides whether the mean is worth anything.
    pub(super) fn worst_load(&self) -> f64 {
        self.kept
            .iter()
            .map(|sample| sample.load_before)
            .fold(0.0, f64::max)
    }
}

/// Compile the crate once without measuring, so its dependencies are built and cached.
pub(super) fn warm_up(target: &Target) -> Result<()> {
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
pub(super) fn order<'a>(variants: &[&'a str], rep: usize) -> Vec<&'a str> {
    let mut ordered = variants.to_vec();
    if rep % 2 == 1 {
        ordered.reverse();
    }
    ordered
}

pub(super) fn measure_once(target: &Target, args: &Args) -> Result<Sample> {
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
    let started = std::time::Instant::now();

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

    let wall_seconds = started.elapsed().as_secs_f64();
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
        wall_seconds,
        pressured,
    })
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct MeasuredCost {
    cpu_seconds: f64,
    peak_rss_bytes: u64,
    ok: bool,
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
pub(super) fn available_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kibibytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kibibytes * 1024)
}

/// Run one command and report what it and its descendants cost.
///
/// This runs in its own process on purpose: `getrusage(RUSAGE_CHILDREN)` reports a high-water mark
/// accumulated over the caller's whole life, so a runner that measured several repetitions itself
/// would report the largest earlier one forever.
#[cfg(unix)]
pub(super) fn measure_child(command: &[String]) -> Result<()> {
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
pub(super) fn measure_child(command: &[String]) -> Result<()> {
    let _ = command;
    bail!(
        "measuring CPU time and peak resident set size needs getrusage, which this platform does \
         not provide; run the benchmark on Linux or macOS"
    )
}

#[cfg(test)]
mod tests {
    use super::{Sample, Summary, order};

    fn sample(cpu: f64, rss: u64, load_before: f64, load_after: f64) -> Sample {
        // Instant, so the repetition is answerable for none of the load rise and the tests below
        // read as statements about the machine rather than about the measurement.
        timed_sample(cpu, rss, load_before, load_after, 0.0)
    }

    fn timed_sample(
        cpu: f64,
        rss: u64,
        load_before: f64,
        load_after: f64,
        wall_seconds: f64,
    ) -> Sample {
        Sample {
            cpu_seconds: cpu,
            peak_rss_bytes: rss,
            load_before,
            load_after,
            wall_seconds,
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
    fn a_repetition_that_spent_its_time_waiting_is_crowded() {
        // Had the processor to itself: 20 CPU seconds in 21 wall seconds.
        assert!(!timed_sample(20.0, 1, 0.10, 0.20, 21.0).crowded());
        // Spent four fifths of its life off the processor.
        assert!(timed_sample(4.0, 1, 0.10, 0.20, 21.0).crowded());
        // `rustc` threading inside one job puts this above 1.0, which is not a problem.
        assert!(!timed_sample(30.0, 1, 0.10, 0.20, 21.0).crowded());
    }

    /// A repetition is not discarded for its own footprint.
    ///
    /// The regression this pins actually happened: measuring `okta` threw away four of six `derive`
    /// repetitions and none of the `hand-written` ones, purely because `derive` takes 21 seconds
    /// against 7 and a 21-second process raises the one-minute load average by about 0.3 all by
    /// itself. Discarding the slow variant *for being slow* biases the comparison the harness
    /// exists to make, towards a bigger win.
    #[test]
    fn a_repetition_is_not_discarded_for_being_slow() {
        // The exact shape that was discarded: load rose 3.02 → 3.61 across a 21-second repetition
        // that was never actually starved. The long one and the short one are now judged alike.
        assert!(!timed_sample(21.3, 1, 3.02, 3.61, 22.0).crowded());
        assert!(!timed_sample(6.9, 1, 3.02, 3.61, 7.4).crowded());
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
    fn the_recorded_load_is_the_worst_repetition_not_the_first() {
        let mut summary = Summary::default();
        summary.kept.push(sample(1.0, 1, 0.40, 0.45));
        summary.kept.push(sample(1.0, 1, 9.10, 9.20));
        summary.kept.push(sample(1.0, 1, 0.50, 0.55));
        // Recording 0.40 here would describe a run that spent a third of itself at load 9 as
        // having been taken on an idle machine.
        assert!((summary.worst_load() - 9.10).abs() < 1e-9);
        assert!(
            !crate::bench::baseline::shortfalls(summary.kept.len(), summary.worst_load(), false)
                .is_empty()
        );
    }

    #[test]
    fn no_kept_repetitions_is_not_an_idle_machine() {
        // `fold(0.0, max)` over nothing is 0.0, which would read as pristine. Nothing is written
        // for an empty summary — `mean_cpu` returns `None` first — and this pins that order.
        let summary = Summary::default();
        assert!((summary.worst_load() - 0.0).abs() < 1e-9);
        assert_eq!(summary.mean_cpu(), None);
    }
}
