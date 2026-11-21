//! What a generated crate costs to compile.
//!
//! Compile cost is a product property here, not an afterthought, so the harness exists before
//! there is anything to benchmark: the measurement discipline is the hard part, and it is the part
//! that has to be right before any number it produces is worth quoting.
//!
//! The discipline, and why each rule is there:
//!
//! * **A-B-B-A ordering** between independent variants, so a drift in machine state over the run
//!   cannot be mistaken for a difference between variants. Members of one workspace stay in
//!   dependency order: reversing those would make one half reuse freshly rebuilt dependencies.
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
//!
//! One module per half: [`plan`] renders the subjects and records what they were, [`measure`]
//! takes one repetition and judges whether it counts, [`baseline`] holds the recorded figures to
//! the discipline, and this file drives them.

pub(crate) mod baseline;
pub(crate) mod measure;
mod plan;

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, WrapErr as _, bail};

use measure::{Profile, Summary};
use plan::{Target, key_of};

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

    /// Measure only the hand-written serde strategy.
    ///
    /// Useful for a packaging A/B after the derive split has already been measured.
    #[arg(long, conflicts_with = "ab")]
    hand_written: bool,

    /// Generate with strict response decoding rather than the lenient default.
    ///
    /// The decoding A/B: lenient decoding adds read forms and their decoders to the type layer,
    /// and this is how their compile cost is measured against the same document without them.
    #[arg(long, conflicts_with = "crate_dir")]
    strict: bool,

    /// Generate the opt-in three-crate workspace and measure every member separately.
    #[arg(long, conflicts_with = "crate_dir")]
    workspace: bool,

    /// Measure the one-crate rendering beside the workspace members for a packaging A/B.
    #[arg(long, requires = "workspace", conflicts_with = "crate_dir")]
    crate_control: bool,

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

    /// Also write the measurements here, in the `customSmallerIsBetter` format that
    /// `github-action-benchmark` keeps a per-commit history of.
    #[arg(long, value_name = "PATH", conflicts_with = "generate_only")]
    json: Option<Utf8PathBuf>,

    /// Also profile one compile of each crate under this valgrind: the instructions it executes,
    /// from cachegrind, and its peak and average heap, from massif.
    ///
    /// Unlike CPU time and peak RSS, these are the same on every processor, so a history recorded
    /// on a pool of different runners can compare them from commit to commit.
    #[arg(long, value_name = "PATH", conflicts_with = "generate_only", value_parser = program)]
    valgrind: Option<Utf8PathBuf>,

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

/// A program given on the command line, as cargo can run it from another directory: a path with a
/// directory is made absolute, while a bare name stays one, to be looked up on `PATH`.
fn program(given: &str) -> Result<Utf8PathBuf, String> {
    let path = Utf8PathBuf::from(given);
    if path.components().count() < 2 {
        return Ok(path);
    }
    let absolute = std::path::absolute(&path).map_err(|error| format!("{given}: {error}"))?;
    Utf8PathBuf::try_from(absolute).map_err(|error| format!("{given}: {error}"))
}

/// The serde strategy each variant renders with.
const DERIVE: &str = "derive";
const HAND_WRITTEN: &str = "hand-written";

fn selected_strategies(ab: bool, hand_written: bool) -> &'static [&'static str] {
    match (ab, hand_written) {
        (true, _) => &[DERIVE, HAND_WRITTEN],
        (false, true) => &[HAND_WRITTEN],
        (false, false) => &[DERIVE],
    }
}

pub fn run(args: &Args) -> eyre::Result<()> {
    if !args.measure.is_empty() {
        return measure::measure_child(&args.measure);
    }
    if args.jobs != 1 {
        println!(
            "warning: --jobs {} makes both numbers depend on scheduling; the established \
             methodology is --jobs 1",
            args.jobs
        );
    }
    crate::generated::require_cargo()?;

    let subjects = plan::plan(args)?;
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
        measure::available_memory().map_or_else(|| "unknown".to_owned(), format_bytes)
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
            measure::warm_up(target)?;
        }
        for rep in 0..args.reps {
            for target in ordered_targets(targets, rep) {
                let variant = &target.variant;
                let sample = measure::measure_once(target, args)?;
                let summary = summaries.entry(key_of(target)).or_default();
                if sample.pressured {
                    summary.pressured += 1;
                    println!(
                        "  rep {rep} {variant}: discarded, memory was thin for a {} peak",
                        format_bytes(sample.peak_rss_bytes)
                    );
                    continue;
                }
                if sample.crowded() {
                    summary.discarded += 1;
                    println!(
                        "  rep {rep} {variant}: discarded, spent {:.0}% of {:.1} s on a processor \
                         (load {:.2} → {:.2})",
                        sample.progress() * 100.0,
                        sample.wall_seconds,
                        sample.load_before,
                        sample.load_after
                    );
                    continue;
                }
                println!(
                    "  rep {rep} {variant}: {:.2} s cpu, {} peak rss",
                    sample.cpu_seconds,
                    format_bytes(sample.peak_rss_bytes),
                );
                summary.kept.push(sample);
            }
        }
    }

    // Profiled once every timed repetition is done, so the instrumented compiles cost the timed
    // ones nothing.
    let profiles = profile_all(&subjects, args.valgrind.as_deref())?;

    report(&summaries);
    report_workspaces(&subjects, &summaries);
    report_packaging(&subjects, &summaries);
    compare(&subjects, &summaries);
    if let Some(path) = &args.json {
        let environment = Environment::detect(args.valgrind.as_deref())?;
        let entries = history_entries(&summaries, &profiles, &environment)?;
        std::fs::write(path, serde_json::to_string_pretty(&entries)?)
            .wrap_err_with(|| format!("writing {path}"))?;
    }
    // Writing or checking a baseline without naming one means the checked-in one: the point of a
    // baseline is that everybody compares against the same file.
    let path = args.baseline.clone().or_else(|| {
        (args.write_baseline || args.check)
            .then(|| crate::paths::corpus_root().join("baseline.toml"))
    });
    if let Some(path) = path {
        let scopes: BTreeMap<String, String> = subjects
            .iter()
            .flat_map(|(_, targets)| targets)
            .map(|target| (key_of(target), target.scope.clone()))
            .collect();
        return baseline::baseline(&path, &scopes, &summaries, args);
    }
    Ok(())
}

/// The valgrind profile of every target, keyed as the summaries are, or none without a valgrind.
fn profile_all(
    subjects: &[(String, Vec<Target>)],
    valgrind: Option<&camino::Utf8Path>,
) -> eyre::Result<BTreeMap<String, Profile>> {
    let mut profiles = BTreeMap::new();
    let Some(valgrind) = valgrind else {
        return Ok(profiles);
    };
    println!();
    for target in subjects.iter().flat_map(|(_, targets)| targets) {
        let profile = measure::profile(target, valgrind)?;
        println!(
            "  {}: {} instructions, {} peak heap, {} average heap",
            key_of(target),
            profile.instructions,
            format_bytes(profile.peak_heap_bytes),
            format_bytes(profile.average_heap_bytes)
        );
        profiles.insert(key_of(target), profile);
    }
    Ok(profiles)
}

/// One point of the history, in `github-action-benchmark`'s `customSmallerIsBetter` format.
#[derive(Debug, serde::Serialize)]
struct HistoryEntry {
    /// The chart the point belongs to, the same from commit to commit.
    name: String,
    unit: &'static str,
    value: f64,
    /// Shown beside the value; half the spread of the repetitions kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<String>,
    /// benchtrail's typed payload: the point's dimensions, metric and environment, and a note.
    extra: String,
}

/// What a point was recorded with: things that move the figures without any change to the code.
#[derive(Debug)]
struct Environment {
    rustc: String,
    /// The processor as the host names it, shown beside each time.
    cpu_name: String,
    /// The processor's short name, [`short_cpu`], which splits each time into one series per model.
    cpu: String,
    /// The valgrind that counted the instructions, when any were counted.
    valgrind: Option<String>,
}

impl Environment {
    fn detect(valgrind: Option<&camino::Utf8Path>) -> eyre::Result<Self> {
        let cpu_name = cpu_model();
        let valgrind = valgrind
            .map(|valgrind| {
                let output = std::process::Command::new(valgrind)
                    .arg("--version")
                    .output()
                    .wrap_err_with(|| format!("running {valgrind}"))?;
                Ok::<_, eyre::Report>(String::from_utf8_lossy(&output.stdout).trim().to_owned())
            })
            .transpose()?;
        Ok(Self {
            rustc: rustc_version(),
            cpu: short_cpu(&cpu_name),
            cpu_name,
            valgrind,
        })
    }
}

/// The measurements as `github-action-benchmark` entries: per variant, its valgrind profile when
/// one was taken, its mean CPU time, and its mean peak resident set size.
///
/// Each entry carries benchtrail's typed payload.
/// Only CPU time records the processor in its environment, so the dashboard splits it into one
/// series per model.
/// The profile, which no model changes, and the peak RSS, which is a question of memory rather than
/// of the model, stay one series each.
/// The compiler is in every environment, because a toolchain update moves every figure.
///
/// A variant that kept fewer than two repetitions fails the run.
/// One sample is a point whose noise nobody can see, and the next commit is compared against it.
/// The range beside each mean is half the spread of what was kept.
fn history_entries(
    summaries: &BTreeMap<String, Summary>,
    profiles: &BTreeMap<String, Profile>,
    environment: &Environment,
) -> eyre::Result<Vec<HistoryEntry>> {
    const MIB: f64 = 1024.0 * 1024.0;
    let mut entries = Vec::new();
    for (key, summary) in summaries {
        let Some((document, strategy)) = key.split_once('.') else {
            bail!("{key} names no variant, so it has no place in the history");
        };
        let kept = summary.kept.len();
        let (Some(cpu), Some(rss)) = (summary.mean_cpu(), summary.mean_rss()) else {
            bail!("{key} kept no repetition, so there is nothing to record for it");
        };
        if kept < 2 {
            bail!("{key} kept {kept} repetition, and a point in the history needs two");
        }
        let typed = |metric: &str, label: &str, recorded: serde_json::Value, text: &str| {
            typed_extra([document, strategy], [metric, label], &recorded, text)
        };
        let rustc = &environment.rustc;

        if let Some(profile) = profiles.get(key) {
            let Some(valgrind) = &environment.valgrind else {
                bail!("{key} has a valgrind profile but no valgrind that took it");
            };
            let recorded = serde_json::json!({"rustc": rustc, "valgrind": valgrind});
            let figures = [
                (
                    "instructions",
                    "Instructions (cachegrind)",
                    "count",
                    as_f64(profile.instructions),
                    "cachegrind",
                ),
                (
                    "peak heap",
                    "Peak heap (massif)",
                    "MiB",
                    as_f64(profile.peak_heap_bytes) / MIB,
                    "massif",
                ),
                (
                    "average heap",
                    "Average heap (massif)",
                    "MiB",
                    as_f64(profile.average_heap_bytes) / MIB,
                    "massif",
                ),
            ];
            for (metric, label, unit, value, tool) in figures {
                entries.push(HistoryEntry {
                    name: format!("{key} {metric}"),
                    unit,
                    value,
                    range: None,
                    extra: typed(
                        metric,
                        label,
                        recorded.clone(),
                        &format!("one {tool} run; {valgrind}; {rustc}"),
                    ),
                });
            }
        }
        let cpu_samples = summary.kept.iter().map(|sample| sample.cpu_seconds);
        let rss_samples = summary
            .kept
            .iter()
            .map(|sample| as_f64(sample.peak_rss_bytes) / MIB);
        let text = format!(
            "{kept} kept, {} discarded; {}; {rustc}",
            summary.discarded + summary.pressured,
            environment.cpu_name
        );
        entries.push(HistoryEntry {
            name: format!("{key} cpu"),
            unit: "s",
            value: cpu,
            range: Some(format!("± {:.2}", half_spread(cpu_samples))),
            extra: typed(
                "cpu",
                "CPU time",
                serde_json::json!({"rustc": rustc, "cpu": environment.cpu}),
                &text,
            ),
        });
        entries.push(HistoryEntry {
            name: format!("{key} peak rss"),
            unit: "MiB",
            value: as_f64(rss) / MIB,
            range: Some(format!("± {:.1}", half_spread(rss_samples))),
            extra: typed(
                "peak rss",
                "Peak RSS",
                serde_json::json!({"rustc": rustc}),
                &text,
            ),
        });
    }
    if entries.is_empty() {
        bail!("nothing was measured, so there is nothing to record");
    }
    Ok(entries)
}

/// benchtrail's typed payload for one point: its `[document, strategy]` dimensions, its
/// `[id, label]` metric, which is always lower-is-better here, the environment it was recorded in,
/// and a note.
fn typed_extra(
    [document, strategy]: [&str; 2],
    [metric, label]: [&str; 2],
    recorded: &serde_json::Value,
    text: &str,
) -> String {
    serde_json::json!({
        "benchtrail": {
            "version": 1,
            "dimensions": {"document": document, "strategy": strategy},
            "metric": {"id": metric, "label": label, "direction": "lower"},
            "environment": recorded,
        },
        "text": text,
    })
    .to_string()
}

/// Half the distance between the smallest and the largest value.
fn half_spread(values: impl Iterator<Item = f64> + Clone) -> f64 {
    let low = values.clone().fold(f64::INFINITY, f64::min);
    let high = values.fold(f64::NEG_INFINITY, f64::max);
    (high - low) / 2.0
}

/// The compiler the measurements were taken with, or a placeholder when it cannot be asked.
///
/// That is the compiler cargo runs: `$RUSTC`, or `rustc` on `PATH`.
fn rustc_version() -> String {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(|| "rustc unknown".to_owned(), |text| text.trim().to_owned())
}

/// The processor's model name from `/proc/cpuinfo`, or `unknown` where there is none.
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, name)| name.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A processor's model name without its noise, which names one series on the dashboard.
///
/// Only noise goes: the vendor word, trademark marks, an Intel name's `CPU @ <frequency>`, and an
/// AMD name's trailing `<N>-Core Processor`, which counts the host's cores rather than the
/// runner's.
/// The family word and every model token stay, so two models never share a name: "AMD EPYC 7763
/// 64-Core Processor" is "EPYC 7763" and "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz" is
/// "Xeon Platinum 8370C".
/// Some hosts write the whole name in capitals; such a name gets the vendor's spelling, so it
/// names the same series.
/// A name from an unknown vendor, or one that would be left empty, stays as it is.
fn short_cpu(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let words: Vec<String> = collapsed
        .split(' ')
        .map(|word| {
            ["(R)", "(r)", "(TM)", "(tm)", "(C)", "(c)"]
                .iter()
                .fold(word.to_owned(), |word, mark| word.replace(mark, ""))
        })
        .filter(|word| !word.is_empty())
        .collect();
    let Some(first) = words.first() else {
        return collapsed;
    };
    let Some(vendor) = ["AMD", "Intel", "Apple"]
        .into_iter()
        .find(|vendor| first.eq_ignore_ascii_case(vendor))
    else {
        return collapsed;
    };
    let shouted = first != vendor && !first.chars().any(|c| c.is_ascii_lowercase());
    let mut model: Vec<String> = words
        .into_iter()
        .skip(1)
        .map(|word| {
            if shouted && word.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut word = word.to_ascii_lowercase();
                if let Some(initial) = word.get_mut(..1) {
                    initial.make_ascii_uppercase();
                }
                word
            } else {
                word
            }
        })
        .collect();
    // `CPU @ 2.80GHz`
    if let [.., cpu, at, frequency] = model.as_slice()
        && cpu.eq_ignore_ascii_case("CPU")
        && at == "@"
        && frequency.to_ascii_lowercase().ends_with("hz")
    {
        model.truncate(model.len().saturating_sub(3));
    }
    // `64-Core Processor` or `Processor`
    if model
        .last()
        .is_some_and(|word| word.eq_ignore_ascii_case("Processor"))
    {
        model.pop();
        if model
            .last()
            .is_some_and(|word| word.to_ascii_lowercase().ends_with("-core"))
        {
            model.pop();
        }
    }
    if model.is_empty() {
        return collapsed;
    }
    model.join(" ")
}

fn ordered_targets(targets: &[Target], rep: usize) -> Vec<&Target> {
    let mut units: Vec<((&str, bool), Vec<&Target>)> = Vec::new();
    for target in targets {
        let key = (target_strategy(target), target.member.is_empty());
        if let Some((_, unit)) = units.iter_mut().find(|(candidate, _)| *candidate == key) {
            unit.push(target);
        } else {
            units.push((key, vec![target]));
        }
    }
    for ((_, is_single_crate), members) in &mut units {
        if !*is_single_crate {
            members.sort_by_key(|target| match target.member.as_str() {
                "types" => 0,
                "client" => 1,
                "server" => 2,
                _ => 3,
            });
        }
    }
    if rep % 2 == 1 {
        units.reverse();
    }
    units.into_iter().flat_map(|(_, targets)| targets).collect()
}

fn report(summaries: &BTreeMap<String, Summary>) {
    println!();
    println!("measurements");
    for (key, summary) in summaries {
        match (summary.mean_cpu(), summary.mean_wall(), summary.mean_rss()) {
            (Some(cpu), Some(wall), Some(rss)) => {
                println!(
                    "  {key:<34} {cpu:>7.2} s cpu   {wall:>7.2} s wall   {:>10} peak rss   \
                     ({} kept, {} discarded)",
                    format_bytes(rss),
                    summary.kept.len(),
                    summary.discarded + summary.pressured
                );
                if summary.pressured > 0 {
                    println!(
                        "    {} pressure-disqualified repetition{}",
                        summary.pressured,
                        if summary.pressured == 1 { "" } else { "s" }
                    );
                }
            }
            _ => println!(
                "  {key:<34} no usable repetitions ({} discarded)",
                summary.discarded + summary.pressured
            ),
        }
    }
}

fn report_workspaces(subjects: &[(String, Vec<Target>)], summaries: &BTreeMap<String, Summary>) {
    let split = subjects
        .iter()
        .filter(|(_, targets)| targets.iter().any(|target| !target.member.is_empty()));
    let mut printed = false;
    for (subject, targets) in split {
        let strategies: BTreeSet<&str> = targets.iter().map(target_strategy).collect();
        for strategy in strategies {
            let members: Vec<&Target> = targets
                .iter()
                .filter(|target| !target.member.is_empty())
                .filter(|target| target_strategy(target) == strategy)
                .collect();
            let costs: Option<Vec<(f64, f64, u64)>> = members
                .iter()
                .map(|target| {
                    let summary = summaries.get(&key_of(target))?;
                    Some((
                        summary.mean_cpu()?,
                        summary.mean_wall()?,
                        summary.mean_rss()?,
                    ))
                })
                .collect();
            let Some(costs) = costs else {
                continue;
            };
            if !printed {
                println!();
                println!("workspace totals");
                printed = true;
            }
            let cpu = costs.iter().map(|(cpu, _, _)| cpu).sum::<f64>();
            let wall = costs.iter().map(|(_, wall, _)| wall).sum::<f64>();
            let worst = costs
                .iter()
                .map(|(_, _, rss)| *rss)
                .max()
                .unwrap_or_default();
            let sum = costs.iter().map(|(_, _, rss)| *rss).sum::<u64>();
            println!(
                "  {subject}.{strategy:<20} worst crate {}, sum of member peaks {}; {:.2} s cpu, \
                 {:.2} s sequential wall across {} invocations",
                format_bytes(worst),
                format_bytes(sum),
                cpu,
                wall,
                members.len(),
            );
        }
    }
}

fn report_packaging(subjects: &[(String, Vec<Target>)], summaries: &BTreeMap<String, Summary>) {
    let mut printed = false;
    let mut totals: BTreeMap<&str, (f64, f64, usize)> = BTreeMap::new();
    for (subject, targets) in subjects {
        let strategies: BTreeSet<&str> = targets.iter().map(target_strategy).collect();
        for strategy in strategies {
            let Some(control) = targets
                .iter()
                .find(|target| target.variant == format!("{strategy}.crate"))
            else {
                continue;
            };
            let members: Vec<&Target> = targets
                .iter()
                .filter(|target| !target.member.is_empty())
                .filter(|target| target_strategy(target) == strategy)
                .collect();
            let Some(control_cost) = summaries.get(&key_of(control)) else {
                continue;
            };
            let member_costs: Option<Vec<&Summary>> = members
                .iter()
                .map(|target| summaries.get(&key_of(target)))
                .collect();
            let Some(member_costs) = member_costs else {
                continue;
            };
            let Some((control_wall, control_rss)) =
                control_cost.mean_wall().zip(control_cost.mean_rss())
            else {
                continue;
            };
            let split_wall: Option<f64> =
                member_costs.iter().map(|summary| summary.mean_wall()).sum();
            let split_worst = member_costs
                .iter()
                .filter_map(|summary| summary.mean_rss())
                .max();
            let Some((split_wall, split_worst)) = split_wall.zip(split_worst) else {
                continue;
            };
            if !printed {
                println!();
                println!("workspace against crate packaging");
                printed = true;
            }
            println!(
                "  {subject}.{strategy:<20} worst rss {:+.1}%, sequential wall {:+.1}% \
                 ({} invocations vs 1)",
                percent(as_f64(control_rss), as_f64(split_worst)),
                percent(control_wall, split_wall),
                members.len(),
            );
            let (control_total, split_total, documents) = totals.entry(strategy).or_default();
            *control_total += control_wall;
            *split_total += split_wall;
            *documents += 1;
        }
    }
    if printed {
        println!("  corpus totals");
        for (strategy, (control_wall, split_wall, documents)) in totals {
            println!(
                "    {strategy:<20} sequential wall {:+.1}% ({control_wall:.2} s → \
                 {split_wall:.2} s across {documents} documents)",
                percent(control_wall, split_wall),
            );
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
    let paired: Vec<(&str, &Target, &Target)> = subjects
        .iter()
        .flat_map(|(subject, targets)| {
            let members: BTreeSet<&str> = targets
                .iter()
                .map(|target| target.member.as_str())
                .collect();
            members.into_iter().filter_map(move |member| {
                let derive = targets
                    .iter()
                    .find(|target| target.member == member && target_strategy(target) == DERIVE)?;
                let hand = targets.iter().find(|target| {
                    target.member == member && target_strategy(target) == HAND_WRITTEN
                })?;
                Some((subject.as_str(), derive, hand))
            })
        })
        .collect();
    if paired.is_empty() {
        return;
    }
    println!();
    println!("hand-written against derive");
    for (subject, derive, hand) in paired {
        let (Some(before), Some(after)) =
            (summaries.get(&key_of(derive)), summaries.get(&key_of(hand)))
        else {
            continue;
        };
        let label = if derive.member.is_empty() {
            subject.to_owned()
        } else {
            format!("{subject}/{}", derive.member)
        };
        let (Some(cpu_before), Some(cpu_after)) = (before.mean_cpu(), after.mean_cpu()) else {
            println!("  {label:<24} no usable repetitions");
            continue;
        };
        let (Some(rss_before), Some(rss_after)) = (before.mean_rss(), after.mean_rss()) else {
            continue;
        };
        println!(
            "  {label:<24} cpu {:+.1}%   peak rss {:+.1}%",
            percent(cpu_before, cpu_after),
            percent(as_f64(rss_before), as_f64(rss_after)),
        );
    }
}

fn target_strategy(target: &Target) -> &str {
    if target.strategy.is_empty() {
        target
            .variant
            .split_once('.')
            .map_or(target.variant.as_str(), |(strategy, _)| strategy)
    } else {
        &target.strategy
    }
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

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", as_f64(bytes) / as_f64(GIB))
    } else {
        format!("{:.1} MiB", as_f64(bytes) / as_f64(MIB))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use color_eyre::eyre;

    use super::measure::{Profile, Sample, Summary};
    use super::{
        Environment, HAND_WRITTEN, Target, format_bytes, history_entries, ordered_targets, percent,
        program, selected_strategies, short_cpu,
    };

    fn target(strategy: &str, member: &str) -> Target {
        Target {
            subject: "fixture".to_owned(),
            variant: format!("{strategy}.{member}"),
            strategy: strategy.to_owned(),
            member: member.to_owned(),
            package: format!("fixture-{member}"),
            directory: camino::Utf8PathBuf::from(format!("/tmp/fixture-{member}")),
            scope: member.to_owned(),
        }
    }

    #[test]
    fn workspace_members_keep_dependency_order_when_units_reverse() {
        let targets = [
            target("derive", "types"),
            target("derive", "client"),
            target("derive", "server"),
            target("hand-written", "types"),
            target("hand-written", "client"),
            target("hand-written", "server"),
        ];

        let forward: Vec<&str> = ordered_targets(&targets, 0)
            .into_iter()
            .map(|target| target.variant.as_str())
            .collect();
        let reverse: Vec<&str> = ordered_targets(&targets, 1)
            .into_iter()
            .map(|target| target.variant.as_str())
            .collect();

        assert_eq!(
            forward,
            [
                "derive.types",
                "derive.client",
                "derive.server",
                "hand-written.types",
                "hand-written.client",
                "hand-written.server",
            ]
        );
        assert_eq!(
            reverse,
            [
                "hand-written.types",
                "hand-written.client",
                "hand-written.server",
                "derive.types",
                "derive.client",
                "derive.server",
            ]
        );
    }

    fn kept(cpu_seconds: f64, peak_rss_bytes: u64) -> Sample {
        Sample {
            cpu_seconds,
            peak_rss_bytes,
            load_before: 0.0,
            load_after: 0.0,
            wall_seconds: cpu_seconds,
            pressured: false,
        }
    }

    /// The environment of a run on an EPYC 7763 whose instructions valgrind 3.22 counted.
    fn environment() -> Environment {
        Environment {
            rustc: "rustc 1.99.0".to_owned(),
            cpu_name: "AMD EPYC 7763 64-Core Processor".to_owned(),
            cpu: "EPYC 7763".to_owned(),
            valgrind: Some("valgrind-3.22.0".to_owned()),
        }
    }

    /// Each variant is up to five points in the history, named so a chart per point lines up
    /// commit after commit: its valgrind profile (instructions, peak heap and average heap), its
    /// mean CPU time, and its mean peak RSS.
    /// Only the CPU time records the processor, so the dashboard can split that one by model.
    #[test_util::test]
    fn a_variant_is_recorded_as_its_count_cpu_time_and_peak() -> eyre::Result<()> {
        let summary = Summary {
            kept: vec![kept(6.0, 1024 * 1024 * 1024), kept(8.0, 1024 * 1024 * 1024)],
            discarded: 1,
            ..Summary::default()
        };
        let summaries = BTreeMap::from([("oxide.hand-written".to_owned(), summary)]);
        let profile = Profile {
            instructions: 42_000_000_000,
            peak_heap_bytes: 1024 * 1024 * 1024,
            average_heap_bytes: 512 * 1024 * 1024,
        };
        let profiles = BTreeMap::from([("oxide.hand-written".to_owned(), profile)]);
        let entries = history_entries(&summaries, &profiles, &environment())?;

        // Compared as JSON, because the JSON is what the action reads, with each typed payload
        // read back out of its string.
        let read: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| {
                let mut value = serde_json::to_value(entry)?;
                value["extra"] = serde_json::from_str(&entry.extra)?;
                Ok::<_, eyre::Report>(value)
            })
            .collect::<Result<_, _>>()?;
        let typed = |metric: &str, label: &str, recorded: serde_json::Value, text: &str| {
            serde_json::json!({
                "benchtrail": {
                    "version": 1,
                    "dimensions": {"document": "oxide", "strategy": "hand-written"},
                    "metric": {"id": metric, "label": label, "direction": "lower"},
                    "environment": recorded,
                },
                "text": text,
            })
        };
        let text = "2 kept, 1 discarded; AMD EPYC 7763 64-Core Processor; rustc 1.99.0";
        assert_eq!(
            read,
            [
                serde_json::json!({
                    "name": "oxide.hand-written instructions",
                    "unit": "count",
                    "value": 42_000_000_000.0,
                    "extra": typed(
                        "instructions",
                        "Instructions (cachegrind)",
                        serde_json::json!({"rustc": "rustc 1.99.0", "valgrind": "valgrind-3.22.0"}),
                        "one cachegrind run; valgrind-3.22.0; rustc 1.99.0",
                    ),
                }),
                serde_json::json!({
                    "name": "oxide.hand-written peak heap",
                    "unit": "MiB",
                    "value": 1024.0,
                    "extra": typed(
                        "peak heap",
                        "Peak heap (massif)",
                        serde_json::json!({"rustc": "rustc 1.99.0", "valgrind": "valgrind-3.22.0"}),
                        "one massif run; valgrind-3.22.0; rustc 1.99.0",
                    ),
                }),
                serde_json::json!({
                    "name": "oxide.hand-written average heap",
                    "unit": "MiB",
                    "value": 512.0,
                    "extra": typed(
                        "average heap",
                        "Average heap (massif)",
                        serde_json::json!({"rustc": "rustc 1.99.0", "valgrind": "valgrind-3.22.0"}),
                        "one massif run; valgrind-3.22.0; rustc 1.99.0",
                    ),
                }),
                serde_json::json!({
                    "name": "oxide.hand-written cpu",
                    "unit": "s",
                    "value": 7.0,
                    "range": "± 1.00",
                    "extra": typed(
                        "cpu",
                        "CPU time",
                        serde_json::json!({"rustc": "rustc 1.99.0", "cpu": "EPYC 7763"}),
                        text,
                    ),
                }),
                serde_json::json!({
                    "name": "oxide.hand-written peak rss",
                    "unit": "MiB",
                    "value": 1024.0,
                    "range": "± 0.0",
                    "extra": typed(
                        "peak rss",
                        "Peak RSS",
                        serde_json::json!({"rustc": "rustc 1.99.0"}),
                        text,
                    ),
                }),
            ]
        );
        Ok(())
    }

    /// A variant that kept fewer than two repetitions fails the run rather than recording a point
    /// whose noise cannot be seen.
    #[test]
    fn a_variant_with_fewer_than_two_kept_is_an_error() {
        for kept_samples in [vec![], vec![kept(6.0, 1024)]] {
            let summary = Summary {
                kept: kept_samples,
                discarded: 2,
                ..Summary::default()
            };
            let summaries = BTreeMap::from([("okta.hand-written".to_owned(), summary)]);
            assert!(history_entries(&summaries, &BTreeMap::new(), &environment()).is_err());
        }
        // Nothing measured at all, as with no repetitions, is no history either.
        assert!(history_entries(&BTreeMap::new(), &BTreeMap::new(), &environment()).is_err());
    }

    /// A program path with a directory is made absolute, because cargo runs it from the generated
    /// crate's directory, and a bare name stays a name for `PATH` to resolve.
    #[test_util::test]
    fn a_program_path_survives_a_change_of_directory() -> eyre::Result<()> {
        assert_eq!(
            program("valgrind").map_err(|error| eyre::eyre!(error))?,
            "valgrind"
        );
        let relative = program("tools/valgrind").map_err(|error| eyre::eyre!(error))?;
        assert!(relative.is_absolute(), "{relative}");
        assert!(relative.ends_with("tools/valgrind"), "{relative}");
        Ok(())
    }

    /// A processor's short name keeps its family and model and drops only the vendor, the marks,
    /// the frequency and the core count, so each model is one series however its host spells it.
    #[test]
    fn a_processor_is_named_by_its_family_and_model() {
        for (raw, short) in [
            ("AMD EPYC 7763 64-Core Processor", "EPYC 7763"),
            ("AMD EPYC 9V74 80-Core Processor", "EPYC 9V74"),
            (
                "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
                "Xeon Platinum 8370C",
            ),
            // Shouted, it is the same model.
            (
                "INTEL(R) XEON(R) PLATINUM 8573C CPU @ 2.80GHZ",
                "Xeon Platinum 8573C",
            ),
            ("Apple M3 Pro", "M3 Pro"),
            // An unknown vendor, or nothing past the vendor, stays as it is.
            ("Neoverse-N1", "Neoverse-N1"),
            ("AMD   Processor", "AMD Processor"),
        ] {
            assert_eq!(short_cpu(raw), short, "{raw}");
        }
    }

    #[test]
    fn hand_written_can_be_selected_without_the_derive_control() {
        assert_eq!(selected_strategies(false, true), [HAND_WRITTEN]);
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
