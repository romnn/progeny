//! The recorded baseline: what may be written down, and what a later run may be compared against.
//!
//! A baseline's whole job is to be compared against later, so the conditions a figure was taken
//! under are part of the record rather than a note somewhere. New entries must meet the
//! [`discipline`]; legacy provisional entries retain their verdict so they cannot become a
//! comparison basis by omission.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use color_eyre::eyre::{self, WrapErr, bail};

use super::measure::Summary;
use super::plan::UNRECORDED;
use super::{Args, as_f64, percent};

/// One recorded measurement, and the conditions it was taken under.
///
/// The conditions are part of the record rather than a note somewhere, because a baseline's whole
/// job is to be compared against later — and a comparison between a number taken on a busy machine
/// and one taken on an idle machine is not a comparison.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BaselineEntry {
    cpu_seconds: f64,
    /// Cheapest-to-most-expensive CPU spread across the kept repetitions.
    #[serde(default, skip_serializing_if = "is_zero")]
    cpu_spread_percent: f64,
    /// Absolute form of [`Self::cpu_spread_percent`], so small crates are not judged on a large
    /// percentage of scheduler-scale time.
    #[serde(default, skip_serializing_if = "is_zero")]
    cpu_spread_seconds: f64,
    /// Mean elapsed time for one isolated invocation. Zero in records that predate persisted wall
    /// time, so they remain readable without pretending they measured it.
    #[serde(default)]
    wall_seconds: f64,
    peak_rss_bytes: u64,
    /// What the measured crate held, as classified by `scope_of`. Two entries with different scopes
    /// are not comparable, and `--check` refuses to pretend otherwise.
    #[serde(default)]
    scope: String,
    /// Repetitions kept, and repetitions thrown away because the machine got busier during them.
    kept: usize,
    discarded: usize,
    /// The busiest a kept repetition started at, and the cores it was spread over.
    load: f64,
    cores: usize,
    /// Set when free memory was thin enough that the peak is a floor rather than a result.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pressured: bool,
    /// Empty when the entry meets the [`discipline`]; otherwise every way it does not.
    ///
    /// A non-empty list makes the entry provisional, and `--check` refuses a provisional entry as
    /// the basis of a comparison: a threshold applied against a figure taken on a busy machine
    /// convicts the machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provisional: Vec<String>,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive a reference"
)]
fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

/// The standard a recorded baseline has to meet before its figures may be quoted as *the* number.
///
/// Deliberately not `--max-load`. That flag is an operator's knob: it says how long *this* run will
/// wait for a window, and raising it is reasonable when the question is "did this get worse" rather
/// than "what is the number". This is the standard [06] holds a recorded baseline to — and the two
/// were the same thing once, so overriding the knob quietly lowered the standard and six entries
/// were written at load 12.7 to 18.2 without a word of complaint. A measurement harness that
/// records numbers it should have refused is worse than no harness, because the numbers read as
/// authoritative to everyone who finds them later.
///
/// [06]: ../../../plan/06-workspace-and-validation.md
pub(crate) mod discipline {
    /// The one-minute load average a repetition may *start* at.
    ///
    /// CPU-seconds are not load-immune despite the unit — memory-bandwidth and cache contention
    /// inflate them, by up to 2.6× at load 17 on this hardware.
    pub const MAX_LOAD: f64 = 5.0;

    /// Repetitions that have to survive discarding, per variant. One is an anecdote and two have no
    /// middle; three is the least that can show a spread at all.
    pub const MIN_KEPT: usize = 3;

    /// Largest cheapest-to-most-expensive CPU spread a recorded compile result may carry.
    ///
    /// The regression threshold is 10%; allowing 25% is deliberately loose enough for a shared
    /// machine while still refusing an obvious host-contention outlier. Load-at-start and
    /// processor-progress checks cannot see memory-bandwidth contention that begins mid-sample:
    /// CPU and wall time inflate together.
    pub const MAX_CPU_SPREAD_PERCENT: f64 = 25.0;

    /// Absolute CPU range that must also be exceeded before spread disqualifies a run.
    ///
    /// A few seconds is scheduler-scale variation for a small generated crate. Requiring both
    /// limits keeps the guard aimed at material host-contention events.
    pub const MIN_MATERIAL_CPU_SPREAD_SECONDS: f64 = 3.0;
}

/// Why a recorded entry may not be quoted, or empty when it meets the [`discipline`].
///
/// Written into the entry rather than checked once and forgotten, so the file carries its own
/// verdict: somebody who finds `baseline.toml` and never runs the harness still sees it.
pub(crate) fn shortfalls(kept: usize, load: f64, pressured: bool) -> Vec<String> {
    let mut reasons = Vec::new();
    if load > discipline::MAX_LOAD {
        reasons.push(format!(
            "taken at load {load:.2}, above the ceiling of {:.2}",
            discipline::MAX_LOAD
        ));
    }
    if kept < discipline::MIN_KEPT {
        reasons.push(format!(
            "{kept} repetition{} kept, {} required",
            if kept == 1 { "" } else { "s" },
            discipline::MIN_KEPT
        ));
    }
    if pressured {
        reasons.push(
            "memory was thin, so the peak resident set size is a floor rather than a result"
                .to_owned(),
        );
    }
    reasons
}

pub(super) fn baseline(
    path: &Utf8PathBuf,
    scopes: &BTreeMap<String, String>,
    summaries: &BTreeMap<String, Summary>,
    args: &Args,
) -> eyre::Result<()> {
    let entries: BTreeMap<String, BaselineEntry> = if path.exists() {
        let text = std::fs::read_to_string(path).wrap_err_with(|| format!("reading {path}"))?;
        toml::from_str(&text).wrap_err_with(|| format!("parsing {path}"))?
    } else {
        BTreeMap::new()
    };
    println!();
    if args.write_baseline {
        return write_entries(path, entries, scopes, summaries);
    }
    check_entries(&entries, scopes, summaries, args)
}

/// Record this run only when every new entry meets the measurement discipline.
fn write_entries(
    path: &Utf8PathBuf,
    mut entries: BTreeMap<String, BaselineEntry>,
    scopes: &BTreeMap<String, String>,
    summaries: &BTreeMap<String, Summary>,
) -> eyre::Result<()> {
    for (key, summary) in summaries {
        let scope = scopes.get(key).map_or(UNRECORDED, String::as_str);
        let entry = disciplined_entry(scope, summary)
            .wrap_err_with(|| format!("refusing to record {key}"))?;
        entries.insert(key.clone(), entry);
    }

    let text = toml::to_string_pretty(&entries).wrap_err("rendering the baseline")?;
    std::fs::write(path, text).wrap_err_with(|| format!("writing {path}"))?;
    println!("baseline written to {path}");
    Ok(())
}

fn disciplined_entry(scope: &str, summary: &Summary) -> eyre::Result<BaselineEntry> {
    let shortfalls = compile_shortfalls(summary);
    if !shortfalls.is_empty() {
        bail!("{}", shortfalls.join("; "));
    }
    let Some(((cpu_seconds, wall_seconds), peak_rss_bytes)) = summary
        .mean_cpu()
        .zip(summary.mean_wall())
        .zip(summary.mean_rss())
    else {
        bail!("no usable repetitions");
    };
    Ok(BaselineEntry {
        cpu_seconds,
        cpu_spread_percent: summary.cpu_spread_percent(),
        cpu_spread_seconds: summary.cpu_spread_seconds(),
        wall_seconds,
        peak_rss_bytes,
        scope: scope.to_owned(),
        kept: summary.kept.len(),
        discarded: summary.discarded + summary.pressured,
        load: summary.worst_load(),
        cores: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        pressured: false,
        provisional: Vec::new(),
    })
}

fn compile_shortfalls(summary: &Summary) -> Vec<String> {
    let mut reasons = shortfalls(summary.kept.len(), summary.worst_load(), false);
    if let Some(reason) =
        cpu_spread_shortfall(summary.cpu_spread_percent(), summary.cpu_spread_seconds())
    {
        reasons.push(reason);
    }
    reasons
}

fn cpu_spread_shortfall(percent: f64, seconds: f64) -> Option<String> {
    (percent > discipline::MAX_CPU_SPREAD_PERCENT
        && seconds > discipline::MIN_MATERIAL_CPU_SPREAD_SECONDS)
        .then(|| {
            format!(
                "kept CPU samples span {percent:.1}% and {seconds:.2} seconds, above the stability \
                 ceilings of {:.1}% and {:.1} seconds",
                discipline::MAX_CPU_SPREAD_PERCENT,
                discipline::MIN_MATERIAL_CPU_SPREAD_SECONDS
            )
        })
}

/// Compare this run against what was recorded, and refuse the comparisons that are not one.
fn check_entries(
    entries: &BTreeMap<String, BaselineEntry>,
    scopes: &BTreeMap<String, String>,
    summaries: &BTreeMap<String, Summary>,
    args: &Args,
) -> eyre::Result<()> {
    let mut regressions = Vec::new();
    let mut refused = Vec::new();
    for (key, summary) in summaries {
        let Some((cpu, rss)) = summary.mean_cpu().zip(summary.mean_rss()) else {
            continue;
        };
        let Some(previous) = entries.get(key) else {
            println!("  {key}: no baseline yet");
            continue;
        };
        let scope = scopes.get(key).map_or(UNRECORDED, String::as_str);
        if let Some(reason) = unusable(previous, scope) {
            println!("  {key}: {reason}");
            refused.push(key.clone());
            continue;
        }

        let cpu_delta = percent(previous.cpu_seconds, cpu);
        let rss_delta = percent(as_f64(previous.peak_rss_bytes), as_f64(rss));
        let wall_delta = summary.mean_wall().and_then(|wall| {
            (previous.wall_seconds > 0.0).then(|| percent(previous.wall_seconds, wall))
        });
        println!(
            "  {key}: cpu {cpu_delta:+.1}%, peak rss {rss_delta:+.1}%{}",
            wall_delta.map_or_else(String::new, |delta| format!(", wall {delta:+.1}%"))
        );
        let load = summary.worst_load();
        // Said every time rather than only on a regression: a reader comparing two numbers is
        // entitled to know whether they were taken under comparable conditions, and finding that
        // out afterwards is what makes a bad comparison expensive.
        if (previous.load - load).abs() > 1.0 {
            println!(
                "    conditions differ: the baseline was taken at load {:.2} over {} cores and \
                 this run at load {load:.2}; a difference of a few points may be the machine",
                previous.load, previous.cores
            );
        }
        let here = compile_shortfalls(summary);
        if !here.is_empty() {
            println!(
                "    this run is itself out of discipline — {}",
                here.join("; ")
            );
        }
        if previous.pressured {
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
            if let Some(delta) = wall_delta
                && delta > args.threshold
            {
                regressions.push(format!("{key}: wall {delta:+.1}%"));
            }
        }
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
    if args.check && !refused.is_empty() {
        bail!(
            "{} of {} measurements have no usable baseline to check against: {}",
            refused.len(),
            summaries.len(),
            refused.join(", ")
        );
    }
    Ok(())
}

/// Why a recorded entry cannot serve as the basis of a comparison, if it cannot.
///
/// Answered before any delta is computed. A percentage against an unusable basis is exactly what
/// gets quoted out of the caveat printed beside it, so the harness declines to produce one at all.
fn unusable(previous: &BaselineEntry, scope: &str) -> Option<String> {
    if !previous.provisional.is_empty() {
        return Some(format!(
            "the recorded baseline is provisional and is not a comparison basis — {}",
            previous.provisional.join("; ")
        ));
    }
    // An absent scope predates the field rather than claiming anything, so it is not refused: the
    // entry was recorded when every crate was the same shape.
    // Said with the remedy, because this is what a renderer gaining a module looks like from
    // here: every entry of the old shape is refused at once, and a bail that only refuses reads
    // as the harness being broken rather than the baseline being stale.
    if !previous.scope.is_empty() && previous.scope != scope {
        return Some(format!(
            "the recorded baseline measured {} and this run measured {scope}, which is not a \
             comparison; the rendering changed shape, so re-record the baseline on the \
             reference machine (`--generate-only`, then `--reuse --write-baseline`, as the \
             README's release steps say)",
            previous.scope
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{discipline, shortfalls};

    fn sample(wall_seconds: f64, load_before: f64) -> crate::bench::measure::Sample {
        sample_with_cpu(1.0, wall_seconds, load_before)
    }

    fn sample_with_cpu(
        cpu_seconds: f64,
        wall_seconds: f64,
        load_before: f64,
    ) -> crate::bench::measure::Sample {
        crate::bench::measure::Sample {
            cpu_seconds,
            peak_rss_bytes: 1024,
            load_before,
            load_after: load_before,
            wall_seconds,
            pressured: false,
        }
    }

    #[test_util::test]
    fn a_disciplined_entry_records_wall_time() {
        let summary = crate::bench::measure::Summary {
            kept: vec![sample(2.0, 0.5), sample(2.5, 0.5), sample(3.0, 0.5)],
            discarded: 0,
            pressured: 0,
        };
        let entry = super::disciplined_entry("types-only", &summary)?;
        assert!((entry.wall_seconds - 2.5).abs() < f64::EPSILON);
    }

    #[test_util::test]
    fn an_undisciplined_entry_is_refused_instead_of_recorded_provisionally() {
        let summary = crate::bench::measure::Summary {
            kept: vec![sample(2.0, discipline::MAX_LOAD + 1.0)],
            discarded: 0,
            pressured: 0,
        };
        assert!(super::disciplined_entry("types-only", &summary).is_err());
    }

    #[test_util::test]
    fn a_run_with_an_obvious_cpu_outlier_cannot_be_recorded() {
        // This is the real Cloudflare server run that exposed the hole. Every sample began below
        // load 5 and made enough processor progress to be kept, but the third consumed more than
        // twice the CPU for identical source. Averaging it would publish host contention as cost.
        let summary = crate::bench::measure::Summary {
            kept: vec![
                sample_with_cpu(108.01, 110.0, 0.5),
                sample_with_cpu(122.24, 124.0, 0.5),
                sample_with_cpu(279.02, 281.0, 0.5),
            ],
            discarded: 0,
            pressured: 0,
        };
        assert!(super::disciplined_entry("server", &summary).is_err());
    }

    #[test_util::test]
    fn a_small_absolute_spread_is_not_an_outlier() {
        // Orb's server varied by more than 25% because the whole compile takes about ten seconds.
        // The absolute range is only 2.49 seconds; refusing it would make the rule a small-crate
        // scheduler-jitter detector rather than a guard against the Cloudflare-class event above.
        let summary = crate::bench::measure::Summary {
            kept: vec![
                sample_with_cpu(9.00, 10.0, 0.5),
                sample_with_cpu(8.84, 10.0, 0.5),
                sample_with_cpu(11.33, 12.0, 0.5),
            ],
            discarded: 0,
            pressured: 0,
        };
        assert!(super::disciplined_entry("server", &summary).is_ok());
    }

    #[test_util::test]
    fn a_measurement_within_the_discipline_says_nothing() {
        assert!(shortfalls(discipline::MIN_KEPT, discipline::MAX_LOAD, false).is_empty());
        assert!(shortfalls(discipline::MIN_KEPT + 1, 0.30, false).is_empty());
    }

    #[test_util::test]
    fn the_conditions_the_recorded_baseline_was_taken_under_are_refused() {
        // The six entries this fix exists for: `okta.hand-written` kept one repetition of three at
        // load 18.18, and every entry ran between 12.68 and 18.18 against a ceiling of 5.
        let reasons = shortfalls(1, 18.18, false);
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("load 18.18"), "{reasons:?}");
        assert!(reasons[0].contains("ceiling of 5.00"), "{reasons:?}");
        assert!(reasons[1].contains("1 repetition kept"), "{reasons:?}");
        assert!(reasons[1].contains("3 required"), "{reasons:?}");

        // Load alone is enough, even with every repetition kept.
        assert_eq!(shortfalls(6, 12.68, false).len(), 1);
    }

    #[test_util::test]
    fn thin_memory_disqualifies_a_measurement_taken_on_a_quiet_machine() {
        // Not a load problem and not a replication problem: the kernel reclaims under pressure, so
        // the peak reads *low* and the artefact points the same way as a win.
        let reasons = shortfalls(discipline::MIN_KEPT, 0.10, true);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("floor"), "{reasons:?}");
    }

    #[test_util::test]
    fn a_discarded_pressured_attempt_does_not_poison_valid_repetitions() {
        let summary = crate::bench::measure::Summary {
            kept: vec![sample(2.0, 0.5), sample(2.1, 0.5), sample(2.2, 0.5)],
            discarded: 0,
            pressured: 1,
        };
        assert!(super::disciplined_entry("types-only", &summary).is_ok());
    }

    #[test_util::test]
    fn a_provisional_entry_survives_the_file_and_stays_provisional() {
        // The record has to carry its own verdict: somebody who finds `baseline.toml` and never
        // runs the harness is exactly the reader the flag exists for.
        let entry = super::BaselineEntry {
            cpu_seconds: 8.30,
            cpu_spread_percent: 0.0,
            cpu_spread_seconds: 0.0,
            wall_seconds: 8.50,
            peak_rss_bytes: 1_003_913_216,
            scope: "types-only".to_owned(),
            kept: 1,
            discarded: 2,
            load: 18.18,
            cores: 48,
            pressured: false,
            provisional: shortfalls(1, 18.18, false),
        };
        let text = toml::to_string_pretty(&entry)?;
        assert!(text.contains("provisional"), "{text}");
        assert!(text.contains("types-only"), "{text}");
        let read: super::BaselineEntry = toml::from_str(&text)?;
        assert_eq!(read.provisional.len(), 2);
        assert_eq!(read.scope, "types-only");
    }

    #[test_util::test]
    fn an_entry_that_meets_the_discipline_carries_no_flag_at_all() {
        // `skip_serializing_if` rather than an empty list in the file: a reader scanning for the
        // word should find it only where it means something.
        let entry = super::BaselineEntry {
            cpu_seconds: 1.0,
            cpu_spread_percent: 0.0,
            cpu_spread_seconds: 0.0,
            wall_seconds: 1.1,
            peak_rss_bytes: 2,
            scope: "types-only".to_owned(),
            kept: 4,
            discarded: 0,
            load: 0.42,
            cores: 48,
            pressured: false,
            provisional: Vec::new(),
        };
        let text = toml::to_string_pretty(&entry)?;
        assert!(!text.contains("provisional"), "{text}");
        assert!(!text.contains("pressured"), "{text}");
    }

    fn entry(scope: &str, provisional: Vec<String>) -> super::BaselineEntry {
        super::BaselineEntry {
            cpu_seconds: 1.0,
            cpu_spread_percent: 0.0,
            cpu_spread_seconds: 0.0,
            wall_seconds: 1.1,
            peak_rss_bytes: 2,
            scope: scope.to_owned(),
            kept: 4,
            discarded: 0,
            load: 0.42,
            cores: 48,
            pressured: false,
            provisional,
        }
    }

    #[test_util::test]
    fn a_provisional_baseline_is_not_a_comparison_basis() {
        let reason = super::unusable(
            &entry("types-only", vec!["taken at load 18.18".to_owned()]),
            "types-only",
        );
        assert!(reason.is_some_and(|reason| reason.contains("load 18.18")));
        assert!(super::unusable(&entry("types-only", Vec::new()), "types-only").is_none());
    }

    #[test_util::test]
    fn a_baseline_from_a_different_scope_is_not_a_comparison_basis() {
        // The whole hazard, as an assertion: the stage-4 figure is about types-only crates, and a
        // run against a crate with a client in it answers a different question at a different
        // denominator. Refusing is the only way the two numbers cannot be subtracted.
        let reason = super::unusable(&entry("types-only", Vec::new()), "types+client");
        assert!(reason.is_some_and(|reason| reason.contains("types-only")));
    }

    #[test_util::test]
    fn a_baseline_recorded_before_scopes_existed_is_still_usable() {
        // Absent is not a claim. Refusing here would strand every entry written before the field,
        // which would make the fix cost more than the defect.
        assert!(super::unusable(&entry("", Vec::new()), "types+client").is_none());
    }

    #[test_util::test]
    fn the_checked_in_baseline_agrees_with_its_own_conditions() {
        // The defect this whole fix exists for was a file whose numbers and whose conditions were
        // both recorded, and which never drew the conclusion. Asserting the *consistency* rather
        // than the current verdict is what makes this survive the corrected take: entries stop
        // being provisional and the test keeps holding.
        let path = crate::paths::corpus_root().join("baseline.toml");
        let text = std::fs::read_to_string(&path)?;
        let entries: std::collections::BTreeMap<String, super::BaselineEntry> =
            toml::from_str(&text)?;
        assert!(!entries.is_empty(), "{path} has no entries");
        for (key, entry) in &entries {
            assert!(
                !entry.scope.is_empty(),
                "{key} does not say what it measured, so nothing can be compared against it"
            );
            let mut expected = shortfalls(entry.kept, entry.load, entry.pressured);
            if let Some(reason) =
                super::cpu_spread_shortfall(entry.cpu_spread_percent, entry.cpu_spread_seconds)
            {
                expected.push(reason);
            }
            assert_eq!(
                entry.provisional, expected,
                "{key} disagrees with its own recorded conditions"
            );
        }
    }
}
