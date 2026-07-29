//! The checked-in runtime baseline and the budget it must satisfy.

use std::collections::BTreeMap;

use camino::Utf8Path;
use color_eyre::eyre::{self, ContextCompat, WrapErr, bail};

use super::{MeanPhase, Strategy, Summary, Workload};

const MAX_WALL_RATIO: f64 = 4.5;
const MAX_ALLOCATIONS_RATIO: f64 = 2.25;
const MAX_PEAK_HEAP_RATIO: f64 = 4.0;

#[derive(serde::Serialize)]
struct RuntimeRecord {
    version: u32,
    workload: WorkloadRecord,
    budget: BudgetRecord,
    derive: StrategyRecord,
    hand_written: StrategyRecord,
    comparison: ComparisonRecord,
}

#[derive(serde::Serialize)]
struct WorkloadRecord {
    scope: &'static str,
    tier: Vec<String>,
    selected_documents: usize,
    documents_with_payloads: usize,
    tier_payloads: usize,
    tier_valid_payloads: usize,
    tier_malformed_payloads: usize,
    uncheckable_payloads: usize,
    valid_payload_bytes: usize,
    malformed_payload_bytes: usize,
    deep_payload_bytes: usize,
    deep_depth: usize,
    deep_items: usize,
    iterations: usize,
}

#[derive(serde::Serialize)]
struct BudgetRecord {
    max_wall_ratio: f64,
    max_allocations_ratio: f64,
    max_peak_heap_ratio: f64,
    malformed_messages_must_match: bool,
    deep_malformed_must_be_rejected: bool,
}

#[derive(serde::Serialize)]
struct StrategyRecord {
    kept: usize,
    discarded: usize,
    worst_load: f64,
    cores: usize,
    pressured: bool,
    valid: PhaseRecord,
    malformed: PhaseRecord,
    malformed_cases: usize,
    malformed_rejected: usize,
    malformed_accepted: usize,
    deep_error: String,
}

#[derive(serde::Serialize)]
struct PhaseRecord {
    wall_seconds_per_pass: f64,
    allocations_per_pass: f64,
    peak_heap_bytes: u64,
}

impl From<MeanPhase> for PhaseRecord {
    fn from(phase: MeanPhase) -> Self {
        Self {
            wall_seconds_per_pass: phase.wall_seconds_per_pass,
            allocations_per_pass: phase.allocations_per_pass,
            peak_heap_bytes: phase.peak_heap_bytes,
        }
    }
}

#[derive(serde::Serialize)]
struct ComparisonRecord {
    valid_wall_ratio: f64,
    valid_allocations_ratio: f64,
    valid_peak_heap_ratio: f64,
    malformed_wall_ratio: f64,
    malformed_allocations_ratio: f64,
    malformed_peak_heap_ratio: f64,
    malformed_messages_equal: usize,
    malformed_outcomes_total: usize,
}

pub(crate) fn write(
    path: &Utf8Path,
    workload: &Workload,
    summaries: &BTreeMap<Strategy, Summary>,
) -> eyre::Result<()> {
    let derive = summary(summaries, Strategy::Derive)?;
    let hand = summary(summaries, Strategy::HandWritten)?;
    let shortfalls = [
        (Strategy::Derive, discipline_shortfalls(derive)),
        (Strategy::HandWritten, discipline_shortfalls(hand)),
    ]
    .into_iter()
    .filter(|(_, reasons)| !reasons.is_empty())
    .map(|(strategy, reasons)| format!("{}: {}", strategy.slug(), reasons.join("; ")))
    .collect::<Vec<_>>();
    if !shortfalls.is_empty() {
        bail!(
            "refusing to record an undisciplined runtime measurement: {}",
            shortfalls.join(" | ")
        );
    }

    let derive_valid = derive
        .valid(workload.iterations)
        .wrap_err("derive has no valid-payload measurement")?;
    let hand_valid = hand
        .valid(workload.iterations)
        .wrap_err("hand-written has no valid-payload measurement")?;
    let derive_malformed = derive
        .malformed(workload.iterations)
        .wrap_err("derive has no malformed-payload measurement")?;
    let hand_malformed = hand
        .malformed(workload.iterations)
        .wrap_err("hand-written has no malformed-payload measurement")?;
    let (comparison, deep_rejected) = compare(
        derive,
        hand,
        derive_valid,
        hand_valid,
        derive_malformed,
        hand_malformed,
    )?;
    let violations = budget_violations(&comparison, deep_rejected);
    if !violations.is_empty() {
        bail!(
            "refusing to record a runtime measurement outside its budget: {}",
            violations.join("; ")
        );
    }

    let record = RuntimeRecord {
        version: 1,
        workload: WorkloadRecord {
            scope: "quick-tier generated types, deserialization only, plus one deep/large fixture",
            tier: workload.selected_documents.clone(),
            selected_documents: workload.selected_documents.len(),
            documents_with_payloads: workload.documents_with_payloads,
            tier_payloads: workload.tier_payloads,
            tier_valid_payloads: workload.tier_valid_payloads,
            tier_malformed_payloads: workload.tier_malformed_payloads,
            uncheckable_payloads: workload.uncheckable_payloads,
            valid_payload_bytes: workload.valid_payload_bytes,
            malformed_payload_bytes: workload.malformed_payload_bytes,
            deep_payload_bytes: workload.deep_payload_bytes,
            deep_depth: super::source::DEEP_DEPTH,
            deep_items: super::source::DEEP_ITEMS,
            iterations: workload.iterations,
        },
        budget: BudgetRecord {
            max_wall_ratio: MAX_WALL_RATIO,
            max_allocations_ratio: MAX_ALLOCATIONS_RATIO,
            max_peak_heap_ratio: MAX_PEAK_HEAP_RATIO,
            malformed_messages_must_match: true,
            deep_malformed_must_be_rejected: true,
        },
        derive: strategy_record(derive, derive_valid, derive_malformed)?,
        hand_written: strategy_record(hand, hand_valid, hand_malformed)?,
        comparison,
    };
    let text = toml::to_string_pretty(&record).wrap_err("rendering the runtime baseline")?;
    std::fs::write(path, text).wrap_err_with(|| format!("writing {path}"))?;
    println!("runtime baseline written to {path}");
    Ok(())
}

fn compare(
    derive: &Summary,
    hand: &Summary,
    derive_valid: MeanPhase,
    hand_valid: MeanPhase,
    derive_malformed: MeanPhase,
    hand_malformed: MeanPhase,
) -> eyre::Result<(ComparisonRecord, bool)> {
    let derive_result = derive
        .result()
        .wrap_err("derive has no kept runtime behavior")?;
    let hand_result = hand
        .result()
        .wrap_err("hand-written has no kept runtime behavior")?;
    let equal = derive_result
        .malformed_outcomes
        .iter()
        .zip(&hand_result.malformed_outcomes)
        .filter(|(left, right)| error_message(left) == error_message(right))
        .count();
    let total = derive_result
        .malformed_outcomes
        .len()
        .max(hand_result.malformed_outcomes.len());
    let comparison = ComparisonRecord {
        valid_wall_ratio: ratio(
            derive_valid.wall_seconds_per_pass,
            hand_valid.wall_seconds_per_pass,
        ),
        valid_allocations_ratio: ratio(
            derive_valid.allocations_per_pass,
            hand_valid.allocations_per_pass,
        ),
        valid_peak_heap_ratio: ratio(
            u64_to_f64(derive_valid.peak_heap_bytes),
            u64_to_f64(hand_valid.peak_heap_bytes),
        ),
        malformed_wall_ratio: ratio(
            derive_malformed.wall_seconds_per_pass,
            hand_malformed.wall_seconds_per_pass,
        ),
        malformed_allocations_ratio: ratio(
            derive_malformed.allocations_per_pass,
            hand_malformed.allocations_per_pass,
        ),
        malformed_peak_heap_ratio: ratio(
            u64_to_f64(derive_malformed.peak_heap_bytes),
            u64_to_f64(hand_malformed.peak_heap_bytes),
        ),
        malformed_messages_equal: equal,
        malformed_outcomes_total: total,
    };
    let deep_rejected = derive_result
        .malformed_outcomes
        .iter()
        .chain(&hand_result.malformed_outcomes)
        .filter(|outcome| outcome.starts_with("deep:"))
        .all(|outcome| !outcome.ends_with(": accepted"));
    Ok((comparison, deep_rejected))
}

fn summary(summaries: &BTreeMap<Strategy, Summary>, strategy: Strategy) -> eyre::Result<&Summary> {
    summaries
        .get(&strategy)
        .wrap_err_with(|| format!("{} has no runtime summary", strategy.slug()))
}

fn discipline_shortfalls(summary: &Summary) -> Vec<String> {
    crate::bench::baseline::shortfalls(
        summary.kept.len(),
        summary.worst_load(),
        summary.pressured > 0,
    )
}

fn strategy_record(
    summary: &Summary,
    valid: MeanPhase,
    malformed: MeanPhase,
) -> eyre::Result<StrategyRecord> {
    let result = summary
        .result()
        .wrap_err("a strategy has no kept runtime behavior")?;
    let deep_error = result
        .malformed_outcomes
        .iter()
        .find(|outcome| outcome.starts_with("deep:"))
        .cloned()
        .wrap_err("the deep malformed outcome was not reported")?;
    Ok(StrategyRecord {
        kept: summary.kept.len(),
        discarded: summary.discarded,
        worst_load: summary.worst_load(),
        cores: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        pressured: summary.pressured > 0,
        valid: valid.into(),
        malformed: malformed.into(),
        malformed_cases: result.malformed_cases,
        malformed_rejected: result.malformed_rejected,
        malformed_accepted: result
            .malformed_cases
            .saturating_sub(result.malformed_rejected),
        deep_error,
    })
}

fn budget_violations(comparison: &ComparisonRecord, deep_rejected: bool) -> Vec<String> {
    let mut violations = Vec::new();
    for (name, value, maximum) in [
        ("valid wall", comparison.valid_wall_ratio, MAX_WALL_RATIO),
        (
            "valid allocations",
            comparison.valid_allocations_ratio,
            MAX_ALLOCATIONS_RATIO,
        ),
        (
            "valid peak heap",
            comparison.valid_peak_heap_ratio,
            MAX_PEAK_HEAP_RATIO,
        ),
        (
            "malformed wall",
            comparison.malformed_wall_ratio,
            MAX_WALL_RATIO,
        ),
        (
            "malformed allocations",
            comparison.malformed_allocations_ratio,
            MAX_ALLOCATIONS_RATIO,
        ),
        (
            "malformed peak heap",
            comparison.malformed_peak_heap_ratio,
            MAX_PEAK_HEAP_RATIO,
        ),
    ] {
        if !value.is_finite() || value > maximum {
            violations.push(format!("{name} is {value:.2}×, budget is {maximum:.2}×"));
        }
    }
    if comparison.malformed_messages_equal != comparison.malformed_outcomes_total {
        violations.push(format!(
            "{} of {} malformed messages differ",
            comparison.malformed_outcomes_total - comparison.malformed_messages_equal,
            comparison.malformed_outcomes_total
        ));
    }
    if !deep_rejected {
        violations.push("the deep malformed payload was accepted".to_owned());
    }
    violations
}

fn error_message(outcome: &str) -> &str {
    outcome
        .split_once(" at line ")
        .map_or(outcome, |(message, _)| message)
}

fn ratio(before: f64, after: f64) -> f64 {
    if before == 0.0 {
        return f64::INFINITY;
    }
    after / before
}

#[expect(
    clippy::cast_precision_loss,
    reason = "heap byte counts are converted only to a coarse ratio against the runtime budget"
)]
fn u64_to_f64(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::{
        ComparisonRecord, MAX_ALLOCATIONS_RATIO, budget_violations, discipline_shortfalls,
        error_message,
    };
    use crate::runtime::Summary;

    fn comparison() -> ComparisonRecord {
        ComparisonRecord {
            valid_wall_ratio: 1.0,
            valid_allocations_ratio: 1.0,
            valid_peak_heap_ratio: 1.0,
            malformed_wall_ratio: 1.0,
            malformed_allocations_ratio: 1.0,
            malformed_peak_heap_ratio: 1.0,
            malformed_messages_equal: 4,
            malformed_outcomes_total: 4,
        }
    }

    #[test]
    fn the_budget_refuses_cost_and_behavior_regressions() {
        assert!(budget_violations(&comparison(), true).is_empty());

        let mut expensive = comparison();
        expensive.valid_allocations_ratio = MAX_ALLOCATIONS_RATIO + 0.01;
        assert!(
            budget_violations(&expensive, true)
                .iter()
                .any(|reason| reason.contains("valid allocations"))
        );

        let mut different = comparison();
        different.malformed_messages_equal -= 1;
        assert!(
            budget_violations(&different, true)
                .iter()
                .any(|reason| reason.contains("malformed messages"))
        );
        assert!(
            budget_violations(&comparison(), false)
                .iter()
                .any(|reason| reason.contains("deep malformed"))
        );
    }

    #[test]
    fn an_incomplete_run_cannot_be_recorded() {
        assert!(
            discipline_shortfalls(&Summary::default())
                .iter()
                .any(|reason| reason.contains("0 repetitions kept"))
        );
    }

    #[test]
    fn buffered_error_offsets_do_not_change_the_message() {
        assert_eq!(
            error_message("deep: invalid type at line 1 column 20"),
            error_message("deep: invalid type at line 1 column 40")
        );
    }
}
