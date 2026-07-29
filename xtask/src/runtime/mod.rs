//! Runtime cost of the two generated serde strategies.
//!
//! Compilation is deliberately outside every sample. Each repetition runs already-built binaries
//! in A-B-B-A order, and those binaries time only deserialization while a counting allocator
//! records allocations and logical peak heap. The outer fresh process supplies CPU time and peak
//! RSS so the compile benchmark's load and memory discipline applies unchanged.

mod record;
mod source;

use std::collections::BTreeMap;
use std::process::Command;

use camino::Utf8PathBuf;
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, ContextCompat, WrapErr, bail};
use progeny::harness::Payload;

use crate::bench::measure::{self, MeasuredCost, Sample};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Corpus documents whose payloads are measured. Defaults to the quick tier.
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,

    /// Repetitions per serde strategy.
    #[arg(long, default_value_t = 4)]
    reps: usize,

    /// How many times each generated binary deserializes its complete payload set per repetition.
    #[arg(long, default_value_t = 1_000)]
    iterations: usize,

    /// Refuse to start a repetition while the one-minute load average is above this.
    #[arg(long, default_value_t = 1.0)]
    max_load: f64,

    /// Minutes to wait for the machine to go quiet before giving up on a repetition.
    #[arg(long, default_value_t = 5)]
    max_wait: u64,

    /// Build the benchmark subjects and stop before warming or measuring them.
    #[arg(long, conflicts_with = "write")]
    generate_only: bool,

    /// Record a disciplined run in `corpus/runtime.toml`.
    #[arg(long)]
    write: bool,

    /// Record somewhere other than `corpus/runtime.toml`.
    #[arg(long, value_name = "PATH", requires = "write")]
    output: Option<Utf8PathBuf>,

    /// Internal: run one subject in a fresh process and report its process-level cost.
    #[arg(
        long,
        num_args = 1..,
        value_name = "COMMAND",
        allow_hyphen_values = true,
        hide = true
    )]
    measure: Vec<String>,
}

#[derive(Debug, Clone)]
struct Case {
    label: String,
    type_name: String,
    json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Strategy {
    Derive,
    HandWritten,
}

impl Strategy {
    const ALL: [Self; 2] = [Self::Derive, Self::HandWritten];

    fn slug(self) -> &'static str {
        match self {
            Self::Derive => "derive",
            Self::HandWritten => "hand-written",
        }
    }

    fn serde_impl(self) -> progeny::SerdeImpl {
        match self {
            Self::Derive => progeny::SerdeImpl::DeriveAlways,
            Self::HandWritten => progeny::SerdeImpl::HandWrittenWhereEligible,
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|strategy| strategy.slug() == slug)
    }
}

#[derive(Debug)]
struct Target {
    subject: String,
    binary: Utf8PathBuf,
    valid_cases: usize,
    malformed_cases: usize,
}

#[derive(Debug)]
struct Workload {
    selected_documents: Vec<String>,
    documents_with_payloads: usize,
    tier_payloads: usize,
    tier_valid_payloads: usize,
    tier_malformed_payloads: usize,
    uncheckable_payloads: usize,
    valid_payload_bytes: usize,
    malformed_payload_bytes: usize,
    deep_payload_bytes: usize,
    iterations: usize,
    targets: BTreeMap<Strategy, Vec<Target>>,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
struct Phase {
    wall_nanos: u64,
    allocations: u64,
    peak_heap_bytes: u64,
}

impl Phase {
    fn merge(&mut self, other: Self) {
        self.wall_nanos = self.wall_nanos.saturating_add(other.wall_nanos);
        self.allocations = self.allocations.saturating_add(other.allocations);
        self.peak_heap_bytes = self.peak_heap_bytes.max(other.peak_heap_bytes);
    }
}

#[derive(Debug, serde::Deserialize)]
struct BinaryReport {
    valid: Phase,
    malformed: Phase,
    malformed_cases: usize,
    malformed_rejected: usize,
    malformed_outcomes: Vec<String>,
}

#[derive(Debug, Default)]
struct Aggregate {
    valid: Phase,
    malformed: Phase,
    valid_cases: usize,
    malformed_cases: usize,
    malformed_rejected: usize,
    malformed_outcomes: Vec<String>,
}

#[derive(Debug)]
struct Observation {
    conditions: Sample,
    result: Aggregate,
}

#[derive(Debug, Default)]
struct Summary {
    kept: Vec<Observation>,
    discarded: usize,
    pressured: usize,
}

#[derive(Debug, Clone, Copy)]
struct MeanPhase {
    wall_seconds_per_pass: f64,
    allocations_per_pass: f64,
    peak_heap_bytes: u64,
}

impl Summary {
    fn push(&mut self, observation: Observation) -> eyre::Result<()> {
        if let Some(first) = self.kept.first()
            && (first.result.valid_cases != observation.result.valid_cases
                || first.result.malformed_cases != observation.result.malformed_cases
                || first.result.malformed_rejected != observation.result.malformed_rejected
                || first.result.malformed_outcomes != observation.result.malformed_outcomes)
        {
            bail!("the runtime subject changed behavior between repetitions");
        }
        if observation.conditions.pressured {
            self.pressured += 1;
        }
        self.kept.push(observation);
        Ok(())
    }

    fn mean_phase(
        &self,
        iterations: usize,
        select: impl Fn(&Aggregate) -> Phase,
    ) -> Option<MeanPhase> {
        let count = u32::try_from(self.kept.len()).ok()?;
        let iterations = u32::try_from(iterations).ok()?;
        if count == 0 || iterations == 0 {
            return None;
        }
        let divisor = f64::from(count) * f64::from(iterations);
        let wall_nanos = self
            .kept
            .iter()
            .map(|observation| select(&observation.result).wall_nanos)
            .map(u64_to_f64)
            .sum::<f64>();
        let allocations = self
            .kept
            .iter()
            .map(|observation| select(&observation.result).allocations)
            .map(u64_to_f64)
            .sum::<f64>();
        let peak_heap_bytes = self
            .kept
            .iter()
            .map(|observation| select(&observation.result).peak_heap_bytes)
            .max()
            .unwrap_or_default();
        Some(MeanPhase {
            wall_seconds_per_pass: wall_nanos / divisor / 1_000_000_000.0,
            allocations_per_pass: allocations / divisor,
            peak_heap_bytes,
        })
    }

    fn valid(&self, iterations: usize) -> Option<MeanPhase> {
        self.mean_phase(iterations, |result| result.valid)
    }

    fn malformed(&self, iterations: usize) -> Option<MeanPhase> {
        self.mean_phase(iterations, |result| result.malformed)
    }

    fn worst_load(&self) -> f64 {
        self.kept
            .iter()
            .map(|observation| observation.conditions.load_before)
            .fold(0.0, f64::max)
    }

    fn result(&self) -> Option<&Aggregate> {
        self.kept.first().map(|observation| &observation.result)
    }
}

pub fn run(args: &Args) -> eyre::Result<()> {
    if !args.measure.is_empty() {
        return measure::measure_child(&args.measure);
    }
    if args.reps == 0 {
        bail!("--reps must be greater than zero");
    }
    if args.iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    crate::generated::require_cargo()?;

    let workload = prepare(args)?;
    println!(
        "runtime: {} tier documents, {} carry {} payloads ({} valid, {} malformed), {} positions \
         uncheckable",
        workload.selected_documents.len(),
        workload.documents_with_payloads,
        workload.tier_payloads,
        workload.tier_valid_payloads,
        workload.tier_malformed_payloads,
        workload.uncheckable_payloads,
    );
    println!(
        "runtime: one {}-byte synthetic body follows {} large fields with a depth-{} tail",
        workload.deep_payload_bytes,
        source::DEEP_ITEMS,
        source::DEEP_DEPTH,
    );
    if args.generate_only {
        for strategy in Strategy::ALL {
            for target in targets(&workload, strategy)? {
                println!(
                    "  {}.{} → {}",
                    target.subject,
                    strategy.slug(),
                    target.binary
                );
            }
        }
        return Ok(());
    }

    for strategy in Strategy::ALL {
        for target in targets(&workload, strategy)? {
            let output = Command::new(&target.binary)
                .output()
                .wrap_err_with(|| format!("warming {}", target.binary))?;
            if !output.status.success() {
                bail!(
                    "the runtime subject {} failed during warm-up: {}",
                    target.subject,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
    }

    let mut summaries: BTreeMap<Strategy, Summary> = BTreeMap::new();
    let variants: Vec<&str> = Strategy::ALL
        .iter()
        .map(|strategy| strategy.slug())
        .collect();
    for rep in 0..args.reps {
        for slug in measure::order(&variants, rep) {
            let strategy =
                Strategy::from_slug(slug).wrap_err_with(|| format!("unknown strategy {slug}"))?;
            let observation = measure_strategy(&workload, strategy, args)?;
            let summary = summaries.entry(strategy).or_default();
            if observation.conditions.crowded() {
                summary.discarded += 1;
                println!(
                    "  rep {rep} {slug}: discarded, received {:.0}% of a processor for {:.2} s",
                    observation.conditions.progress() * 100.0,
                    observation.conditions.wall_seconds,
                );
                continue;
            }
            let valid = observation.result.valid;
            let malformed = observation.result.malformed;
            println!(
                "  rep {rep} {slug}: valid {:.3} s / {} alloc / {}; malformed {:.3} s / {} alloc \
                 / {}",
                seconds(valid.wall_nanos),
                valid.allocations,
                format_bytes(valid.peak_heap_bytes),
                seconds(malformed.wall_nanos),
                malformed.allocations,
                format_bytes(malformed.peak_heap_bytes),
            );
            summary.push(observation)?;
        }
    }

    report(&workload, &summaries);
    if args.write {
        let path = args
            .output
            .clone()
            .unwrap_or_else(|| crate::paths::corpus_root().join("runtime.toml"));
        record::write(&path, &workload, &summaries)?;
    }
    Ok(())
}

fn prepare(args: &Args) -> eyre::Result<Workload> {
    let wanted = if args.specs.is_empty() {
        crate::corpus::quick_tier()?
    } else {
        args.specs.clone()
    };
    let documents = crate::corpus::selected(&wanted)?;
    let mut workload = Workload {
        selected_documents: wanted,
        documents_with_payloads: 0,
        tier_payloads: 0,
        tier_valid_payloads: 0,
        tier_malformed_payloads: 0,
        uncheckable_payloads: 0,
        valid_payload_bytes: 0,
        malformed_payload_bytes: 0,
        deep_payload_bytes: 0,
        iterations: args.iterations,
        targets: BTreeMap::new(),
    };

    for (spec, bytes) in &documents {
        let mut config = crate::corpus::config_for(spec);
        config.emit.client = false;
        config.emit.server = false;
        let payloads = progeny::harness::payloads(bytes, &config)
            .wrap_err_with(|| format!("collecting runtime payloads from {}", spec.name))?;
        workload.uncheckable_payloads += payloads.opaque + payloads.unnamed + payloads.captures;
        workload.tier_payloads += payloads.payloads.len();
        if !payloads.payloads.is_empty() {
            workload.documents_with_payloads += 1;
        }
        let (valid, malformed) = cases(&spec.name, &payloads.payloads);
        workload.tier_valid_payloads += valid.len();
        workload.tier_malformed_payloads += malformed.len();
        workload.valid_payload_bytes += valid.iter().map(|case| case.json.len()).sum::<usize>();
        workload.malformed_payload_bytes +=
            malformed.iter().map(|case| case.json.len()).sum::<usize>();
        if !valid.is_empty() || !malformed.is_empty() {
            prepare_subject(
                &spec.name,
                bytes,
                &config,
                &valid,
                &malformed,
                args.iterations,
                &mut workload.targets,
            )?;
        }
    }

    let deep = source::deep_fixture()?;
    let mut config = progeny::Config::default();
    config.emit.client = false;
    config.emit.server = false;
    config.package = progeny::Package {
        name: "runtime-deep".to_owned(),
        version: "0.0.0".to_owned(),
    };
    let payloads = progeny::harness::payloads(&deep.document, &config)
        .wrap_err("collecting the deep runtime payload")?;
    let payload = payloads
        .payloads
        .first()
        .wrap_err("the deep runtime fixture produced no named payload")?;
    let valid = vec![case_of("deep", payload)];
    let malformed = vec![Case {
        label: "deep:/tail/.../count".to_owned(),
        type_name: payload.type_name.clone(),
        json: deep.malformed,
    }];
    workload.deep_payload_bytes = deep.payload_bytes;
    workload.valid_payload_bytes += valid.iter().map(|case| case.json.len()).sum::<usize>();
    workload.malformed_payload_bytes += malformed.iter().map(|case| case.json.len()).sum::<usize>();
    prepare_subject(
        "deep",
        &deep.document,
        &config,
        &valid,
        &malformed,
        args.iterations,
        &mut workload.targets,
    )?;
    Ok(workload)
}

fn cases(subject: &str, payloads: &[Payload]) -> (Vec<Case>, Vec<Case>) {
    let mut valid = Vec::new();
    let mut malformed = Vec::new();
    for payload in payloads {
        let case = case_of(subject, payload);
        if payload.vendor_defect {
            malformed.push(case);
        } else {
            valid.push(case);
        }
    }
    (valid, malformed)
}

fn case_of(subject: &str, payload: &Payload) -> Case {
    Case {
        label: format!("{subject}:{}", payload.location),
        type_name: payload.type_name.clone(),
        json: payload.original.to_string(),
    }
}

fn prepare_subject(
    subject: &str,
    bytes: &[u8],
    base_config: &progeny::Config,
    valid: &[Case],
    malformed: &[Case],
    iterations: usize,
    targets: &mut BTreeMap<Strategy, Vec<Target>>,
) -> eyre::Result<()> {
    for strategy in Strategy::ALL {
        let mut config = base_config.clone();
        config.serde_impl = strategy.serde_impl();
        let output = progeny::generate(bytes, &config)
            .wrap_err_with(|| format!("generating {subject} for {}", strategy.slug()))?;
        let directory =
            crate::generated::write(&format!("runtime-{subject}-{}", strategy.slug()), &output)?;
        let binary_name = format!(
            "runtime_{}_{}",
            crate::corpus::lib_name(&config.package.name),
            strategy.slug().replace('-', "_")
        );
        let binary_dir = directory.join("src/bin");
        std::fs::create_dir_all(&binary_dir).wrap_err_with(|| format!("creating {binary_dir}"))?;
        let binary_source = binary_dir.join(format!("{binary_name}.rs"));
        std::fs::write(
            &binary_source,
            source::render(&config.package.name, valid, malformed, iterations),
        )
        .wrap_err_with(|| format!("writing {binary_source}"))?;
        let status = crate::generated::cargo(&directory)
            .args(["build", "--release", "--quiet", "--bin", &binary_name])
            .status()
            .wrap_err_with(|| format!("building {binary_name}"))?;
        if !status.success() {
            bail!(
                "the runtime subject {subject}.{} did not build",
                strategy.slug()
            );
        }
        let binary = crate::generated::shared_target()
            .join("release")
            .join(format!("{binary_name}{}", std::env::consts::EXE_SUFFIX));
        targets.entry(strategy).or_default().push(Target {
            subject: subject.to_owned(),
            binary,
            valid_cases: valid.len(),
            malformed_cases: malformed.len(),
        });
    }
    Ok(())
}

fn measure_strategy(
    workload: &Workload,
    strategy: Strategy,
    args: &Args,
) -> eyre::Result<Observation> {
    measure::wait_for_quiet(args.max_load, args.max_wait)?;
    let available_before = measure::available_memory();
    let load_before = measure::load_average()?;
    let started = std::time::Instant::now();
    let mut result = Aggregate::default();
    let mut cpu_seconds = 0.0;
    let mut peak_rss_bytes = 0u64;
    for target in targets(workload, strategy)? {
        let (report, process) = run_target(target)?;
        if report.malformed_cases != target.malformed_cases
            || report.malformed_outcomes.len() != target.malformed_cases
        {
            bail!(
                "{}.{} reported {} malformed cases for {} generated cases",
                target.subject,
                strategy.slug(),
                report.malformed_cases,
                target.malformed_cases
            );
        }
        result.valid.merge(report.valid);
        result.malformed.merge(report.malformed);
        result.valid_cases += target.valid_cases;
        result.malformed_cases += target.malformed_cases;
        result.malformed_rejected += report.malformed_rejected;
        result.malformed_outcomes.extend(report.malformed_outcomes);
        cpu_seconds += process.cpu_seconds;
        peak_rss_bytes = peak_rss_bytes.max(process.peak_rss_bytes);
    }
    let wall_seconds = started.elapsed().as_secs_f64();
    let load_after = measure::load_average()?;
    let pressured =
        available_before.is_some_and(|available| available < peak_rss_bytes.saturating_mul(2));
    Ok(Observation {
        conditions: Sample {
            cpu_seconds,
            peak_rss_bytes,
            load_before,
            load_after,
            wall_seconds,
            pressured,
        },
        result,
    })
}

fn run_target(target: &Target) -> eyre::Result<(BinaryReport, MeasuredCost)> {
    let runner = std::env::current_exe().wrap_err("locating this executable")?;
    let output = Command::new(runner)
        .arg("bench-runtime")
        .arg("--measure")
        .arg(&target.binary)
        .output()
        .wrap_err_with(|| format!("measuring {}", target.binary))?;
    if !output.status.success() {
        bail!(
            "the measuring process for {} failed: {}",
            target.subject,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut runtime = None;
    let mut process = None;
    for line in stdout.lines() {
        if let Some(json) = line.strip_prefix("RUNTIME ") {
            runtime = Some(
                serde_json::from_str(json)
                    .wrap_err_with(|| format!("reading {}'s runtime report", target.subject))?,
            );
        } else if line.starts_with('{') {
            process = Some(
                serde_json::from_str(line)
                    .wrap_err_with(|| format!("reading {}'s process report", target.subject))?,
            );
        }
    }
    let runtime = runtime.wrap_err_with(|| {
        format!(
            "{} produced no runtime report:\n{}",
            target.subject,
            stdout.trim()
        )
    })?;
    let process: MeasuredCost =
        process.wrap_err_with(|| format!("{} produced no process report", target.subject))?;
    if !process.ok {
        bail!("the runtime subject {} failed", target.subject);
    }
    Ok((runtime, process))
}

fn targets(workload: &Workload, strategy: Strategy) -> eyre::Result<&[Target]> {
    workload
        .targets
        .get(&strategy)
        .map(Vec::as_slice)
        .wrap_err_with(|| format!("no {} runtime targets were generated", strategy.slug()))
}

fn report(workload: &Workload, summaries: &BTreeMap<Strategy, Summary>) {
    println!();
    println!("runtime measurements, one complete payload pass");
    for strategy in Strategy::ALL {
        let Some(summary) = summaries.get(&strategy) else {
            continue;
        };
        let Some(valid) = summary.valid(workload.iterations) else {
            println!(
                "  {:<14} no usable repetitions ({} discarded)",
                strategy.slug(),
                summary.discarded
            );
            continue;
        };
        let Some(malformed) = summary.malformed(workload.iterations) else {
            continue;
        };
        println!(
            "  {:<14} valid {:.3} ms, {:.0} allocations, {}; malformed {:.3} ms, {:.0} \
             allocations, {} ({} kept, {} discarded)",
            strategy.slug(),
            valid.wall_seconds_per_pass * 1_000.0,
            valid.allocations_per_pass,
            format_bytes(valid.peak_heap_bytes),
            malformed.wall_seconds_per_pass * 1_000.0,
            malformed.allocations_per_pass,
            format_bytes(malformed.peak_heap_bytes),
            summary.kept.len(),
            summary.discarded,
        );
    }
}

fn seconds(nanos: u64) -> f64 {
    u64_to_f64(nanos) / 1_000_000_000.0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "runtime counters become means and ratios; sub-unit precision above 2^53 is irrelevant"
)]
fn u64_to_f64(value: u64) -> f64 {
    value as f64
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", u64_to_f64(bytes) / u64_to_f64(MIB))
    } else {
        format!("{:.1} KiB", u64_to_f64(bytes) / 1024.0)
    }
}
