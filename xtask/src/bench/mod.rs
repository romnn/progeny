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
use color_eyre::eyre;

use measure::Summary;
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

/// The serde strategy each variant renders with.
const DERIVE: &str = "derive";
const HAND_WRITTEN: &str = "hand-written";

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
            for variant in measure::order(&variants, rep) {
                let Some(target) = targets.iter().find(|target| target.variant == variant) else {
                    continue;
                };
                let sample = measure::measure_once(target, args)?;
                let summary = summaries.entry(key_of(target)).or_default();
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
    report_workspaces(&subjects, &summaries);
    report_packaging(&subjects, &summaries);
    compare(&subjects, &summaries);
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
        let pressured = before.pressured > 0 || after.pressured > 0;
        println!(
            "  {label:<24} cpu {:+.1}%   peak rss {:+.1}%{}",
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
    use super::{format_bytes, percent};

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
