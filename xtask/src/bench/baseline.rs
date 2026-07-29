//! The recorded baseline: what may be written down, and what a later run may be compared against.
//!
//! A baseline's whole job is to be compared against later, so the conditions a figure was taken
//! under are part of the record rather than a note somewhere — and an entry that fails the
//! [`discipline`] carries its own verdict in the file, where somebody who never runs the harness
//! still sees it.

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
/// and one taken on an idle machine is not a comparison. The rule this encodes: a baseline may be
/// written on a shared machine, but it may never be *silently* written on one.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BaselineEntry {
    cpu_seconds: f64,
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

/// Record this run, with each entry's verdict on itself.
fn write_entries(
    path: &Utf8PathBuf,
    mut entries: BTreeMap<String, BaselineEntry>,
    scopes: &BTreeMap<String, String>,
    summaries: &BTreeMap<String, Summary>,
) -> eyre::Result<()> {
    let mut provisional = 0usize;
    for (key, summary) in summaries {
        let Some((cpu, rss)) = summary.mean_cpu().zip(summary.mean_rss()) else {
            continue;
        };
        let load = summary.worst_load();
        let shortfalls = shortfalls(summary.kept.len(), load, summary.pressured > 0);
        if !shortfalls.is_empty() {
            provisional += 1;
            println!("  {key}: provisional — {}", shortfalls.join("; "));
        }
        entries.insert(
            key.clone(),
            BaselineEntry {
                cpu_seconds: cpu,
                peak_rss_bytes: rss,
                scope: scopes
                    .get(key)
                    .map_or(UNRECORDED, String::as_str)
                    .to_owned(),
                kept: summary.kept.len(),
                discarded: summary.discarded,
                load,
                cores: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
                pressured: summary.pressured > 0,
                provisional: shortfalls,
            },
        );
    }

    let text = toml::to_string_pretty(&entries).wrap_err("rendering the baseline")?;
    std::fs::write(path, text).wrap_err_with(|| format!("writing {path}"))?;
    println!("baseline written to {path}");
    if provisional > 0 {
        // Written and marked rather than refused outright: a directional row taken on a shared
        // machine is worth having, and the failure this guards against is not recording one — it
        // is recording one that reads like the finished measurement.
        println!(
            "  {provisional} of {} entries are provisional and may not be quoted as the number; \
             re-take them with the load at or below {:.2} and at least {} repetitions kept",
            entries.len(),
            discipline::MAX_LOAD,
            discipline::MIN_KEPT
        );
    }
    Ok(())
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
        println!("  {key}: cpu {cpu_delta:+.1}%, peak rss {rss_delta:+.1}%");
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
        let here = shortfalls(summary.kept.len(), load, summary.pressured > 0);
        if !here.is_empty() {
            println!(
                "    this run is itself out of discipline — {}",
                here.join("; ")
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
    if !previous.scope.is_empty() && previous.scope != scope {
        return Some(format!(
            "the recorded baseline measured {} and this run measured {scope}, which is not a \
             comparison",
            previous.scope
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{discipline, shortfalls};

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
    fn a_provisional_entry_survives_the_file_and_stays_provisional() {
        // The record has to carry its own verdict: somebody who finds `baseline.toml` and never
        // runs the harness is exactly the reader the flag exists for.
        let entry = super::BaselineEntry {
            cpu_seconds: 8.30,
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
            assert_eq!(
                entry.provisional,
                shortfalls(entry.kept, entry.load, entry.pressured),
                "{key} disagrees with its own recorded conditions"
            );
        }
    }
}
