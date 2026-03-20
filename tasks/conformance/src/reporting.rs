use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::fixtures::{ActualState, ExpectedState, FixtureOutcome, FixtureResult, Status};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCategory {
    NoOutput,
    FalseMemo,
    WrongCache,
    SameCacheBodyDiff,
    MissingMemo,
    NeitherMemo,
    Timeout,
    UnexpectedSkip,
    UnexpectedError,
    HarnessFailure,
    StateMismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FailureDiffLine {
    pub(crate) line: usize,
    pub(crate) actual: String,
    pub(crate) expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FailureRecord {
    pub(crate) name: String,
    pub(crate) category: FailureCategory,
    #[serde(default)]
    pub(crate) expected_state: Option<ExpectedState>,
    #[serde(default)]
    pub(crate) actual_state: Option<ActualState>,
    #[serde(default)]
    pub(crate) parity_success: bool,
    pub(crate) actual_cache_size: Option<u32>,
    pub(crate) expected_cache_size: Option<u32>,
    pub(crate) message: Option<String>,
    pub(crate) is_error_fixture: bool,
    pub(crate) diff_lines: Vec<FailureDiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SkipRecord {
    pub(crate) name: String,
    pub(crate) message: Option<String>,
    pub(crate) is_error_fixture: bool,
    #[serde(default)]
    pub(crate) expected_state: Option<ExpectedState>,
    #[serde(default)]
    pub(crate) actual_state: Option<ActualState>,
    #[serde(default)]
    pub(crate) outcome: Option<FixtureOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FailureJsonReport {
    pub(crate) total: usize,
    #[serde(default)]
    pub(crate) parity_success: usize,
    #[serde(default)]
    pub(crate) parity_failure: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
    pub(crate) include_errors: bool,
    pub(crate) failures: Vec<FailureRecord>,
    #[serde(default)]
    pub(crate) skips_details: Vec<SkipRecord>,
}

/// Check if code has a memo cache call: `_c(N)` or `_c2(N)` etc.
pub(crate) fn has_memo_cache(code: &str) -> bool {
    let bytes = code.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i] == b'_' && bytes[i + 1] == b'c' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len()
                && bytes[j] == b'('
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn extract_cache_size(code: &str) -> Option<u32> {
    let bytes = code.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i] == b'_' && bytes[i + 1] == b'c' {
            if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
                continue;
            }
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                let start = j + 1;
                if let Some(end) = code[start..].find(')') {
                    return code[start..start + end].parse().ok();
                }
            }
        }
    }
    None
}

pub(crate) fn is_transformed_output_mismatch(result: &FixtureResult) -> bool {
    matches!(result.status, Status::Fail)
        && matches!(result.outcome, FixtureOutcome::Mismatch)
        && matches!(result.expected_state, Some(ExpectedState::Transform))
        && matches!(result.actual_state, ActualState::Transformed)
        && result.actual_code.is_some()
        && result.expected_code.is_some()
}

pub(crate) fn categorize_failure(result: &FixtureResult) -> FailureCategory {
    match result.outcome {
        FixtureOutcome::Timeout => return FailureCategory::Timeout,
        FixtureOutcome::UnexpectedSkip => return FailureCategory::UnexpectedSkip,
        FixtureOutcome::UnexpectedError => return FailureCategory::UnexpectedError,
        FixtureOutcome::HarnessFailure => return FailureCategory::HarnessFailure,
        FixtureOutcome::Mismatch => {
            if !is_transformed_output_mismatch(result) {
                return FailureCategory::StateMismatch;
            }
        }
        _ => return FailureCategory::StateMismatch,
    }
    let actual = result.actual_code.as_deref().unwrap_or("");
    let expected = result.expected_code.as_deref().unwrap_or("");
    let actual_has_c = has_memo_cache(actual);
    let expected_has_c = has_memo_cache(expected);

    if actual.trim().is_empty() {
        FailureCategory::NoOutput
    } else if actual_has_c && !expected_has_c {
        FailureCategory::FalseMemo
    } else if !actual_has_c && expected_has_c {
        FailureCategory::MissingMemo
    } else if !actual_has_c && !expected_has_c {
        FailureCategory::NeitherMemo
    } else {
        let actual_cache = extract_cache_size(actual);
        let expected_cache = extract_cache_size(expected);
        if actual_cache != expected_cache {
            FailureCategory::WrongCache
        } else {
            FailureCategory::SameCacheBodyDiff
        }
    }
}

fn collect_first_diff_lines(
    actual: &str,
    expected: &str,
    max_lines: usize,
) -> Vec<FailureDiffLine> {
    let actual_lines: Vec<&str> = actual.lines().collect();
    let expected_lines: Vec<&str> = expected.lines().collect();
    let max = actual_lines.len().max(expected_lines.len());
    let mut diffs = Vec::new();
    for i in 0..max {
        let a = actual_lines.get(i).unwrap_or(&"");
        let e = expected_lines.get(i).unwrap_or(&"");
        if a != e {
            diffs.push(FailureDiffLine {
                line: i + 1,
                actual: (*a).to_string(),
                expected: (*e).to_string(),
            });
            if diffs.len() >= max_lines {
                break;
            }
        }
    }
    diffs
}

pub(crate) fn build_failure_report(
    results: &[FixtureResult],
    include_errors: bool,
    parity_success: usize,
    parity_failure: usize,
    skipped: usize,
) -> FailureJsonReport {
    let failures = results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail))
        .map(|r| {
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            FailureRecord {
                name: r.name.clone(),
                category: categorize_failure(r),
                expected_state: r.expected_state,
                actual_state: Some(r.actual_state),
                parity_success: r.parity_success,
                actual_cache_size: extract_cache_size(actual),
                expected_cache_size: extract_cache_size(expected),
                message: r.message.clone(),
                is_error_fixture: r.is_error_fixture,
                diff_lines: if is_transformed_output_mismatch(r) {
                    collect_first_diff_lines(actual, expected, 5)
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    let skips_details = results
        .iter()
        .filter(|r| matches!(r.status, Status::Skip))
        .map(|r| SkipRecord {
            name: r.name.clone(),
            message: r.message.clone(),
            is_error_fixture: r.is_error_fixture,
            expected_state: r.expected_state,
            actual_state: Some(r.actual_state),
            outcome: Some(r.outcome),
        })
        .collect();

    FailureJsonReport {
        total: results.len(),
        parity_success,
        parity_failure,
        passed: parity_success,
        failed: parity_failure,
        skipped,
        include_errors,
        failures,
        skips_details,
    }
}

pub(crate) fn write_failure_json_report(
    path: &Path,
    report: &FailureJsonReport,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed creating parent directory for {}: {e}",
                path.display()
            )
        })?;
    }
    let json = serde_json::to_string_pretty(report).map_err(|e| {
        format!(
            "failed serializing failures report for {}: {e}",
            path.display()
        )
    })?;
    std::fs::write(path, json).map_err(|e| format!("failed writing {}: {e}", path.display()))
}

pub(crate) fn print_regression_vs(
    baseline_path: &Path,
    current: &FailureJsonReport,
) -> Result<(), String> {
    let baseline_raw = std::fs::read_to_string(baseline_path)
        .map_err(|e| format!("failed reading baseline {}: {e}", baseline_path.display()))?;
    let baseline: FailureJsonReport = serde_json::from_str(&baseline_raw).map_err(|e| {
        format!(
            "failed parsing baseline json {}: {e}",
            baseline_path.display()
        )
    })?;

    let mut baseline_name_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut current_name_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut baseline_category_counts: BTreeMap<String, BTreeMap<FailureCategory, usize>> =
        BTreeMap::new();
    let mut current_category_counts: BTreeMap<String, BTreeMap<FailureCategory, usize>> =
        BTreeMap::new();

    for failure in &baseline.failures {
        *baseline_name_counts
            .entry(failure.name.clone())
            .or_insert(0) += 1;
        *baseline_category_counts
            .entry(failure.name.clone())
            .or_default()
            .entry(failure.category.clone())
            .or_insert(0) += 1;
    }
    for failure in &current.failures {
        *current_name_counts.entry(failure.name.clone()).or_insert(0) += 1;
        *current_category_counts
            .entry(failure.name.clone())
            .or_default()
            .entry(failure.category.clone())
            .or_insert(0) += 1;
    }

    let mut all_names: BTreeSet<String> = BTreeSet::new();
    all_names.extend(baseline_name_counts.keys().cloned());
    all_names.extend(current_name_counts.keys().cloned());

    let mut added_entries: Vec<(String, usize)> = Vec::new();
    let mut removed_entries: Vec<(String, usize)> = Vec::new();
    let mut changed_category: Vec<String> = Vec::new();

    for name in &all_names {
        let baseline_count = *baseline_name_counts.get(name).unwrap_or(&0);
        let current_count = *current_name_counts.get(name).unwrap_or(&0);
        if current_count > baseline_count {
            added_entries.push((name.clone(), current_count - baseline_count));
        } else if baseline_count > current_count {
            removed_entries.push((name.clone(), baseline_count - current_count));
        }

        if baseline_count > 0 && current_count > 0 {
            let baseline_hist = baseline_category_counts.get(name);
            let current_hist = current_category_counts.get(name);
            if baseline_hist != current_hist {
                changed_category.push(name.clone());
            }
        }
    }

    let added_total: usize = added_entries.iter().map(|(_, n)| *n).sum();
    let removed_total: usize = removed_entries.iter().map(|(_, n)| *n).sum();

    let format_hist = |hist: Option<&BTreeMap<FailureCategory, usize>>| -> String {
        match hist {
            None => "-".to_string(),
            Some(hist) => hist
                .iter()
                .map(|(cat, count)| format!("{cat:?}x{count}"))
                .collect::<Vec<_>>()
                .join(","),
        }
    };

    println!(
        "\n=== Regression-vs baseline ({}) ===",
        baseline_path.display()
    );
    println!("Baseline failed: {}", baseline.failures.len());
    println!("Current failed: {}", current.failures.len());
    println!("Added failures: {}", added_total);
    println!("Removed failures: {}", removed_total);
    println!("Changed category: {}", changed_category.len());

    if added_entries.len() <= 50 {
        for (name, count) in &added_entries {
            println!(
                "  + {} (x{}) [{}]",
                name,
                count,
                format_hist(current_category_counts.get(name))
            );
        }
    }
    if removed_entries.len() <= 50 {
        for (name, count) in &removed_entries {
            println!(
                "  - {} (x{}) [{}]",
                name,
                count,
                format_hist(baseline_category_counts.get(name))
            );
        }
    }
    if changed_category.len() <= 50 {
        for name in &changed_category {
            println!(
                "  ~ {} [{} -> {}]",
                name,
                format_hist(baseline_category_counts.get(name)),
                format_hist(current_category_counts.get(name))
            );
        }
    }
    Ok(())
}

pub(crate) fn generate_snapshot(results: &[FixtureResult], pass_rate: f64) -> String {
    let mut snap = String::new();
    snap.push_str(&format!(
        "# React Compiler Conformance -- {pass_rate:.1}% parity rate\n\n"
    ));

    let mut pass_count = 0;
    let mut fail_count = 0;
    let mut skip_count = 0;

    for r in results {
        match r.status {
            Status::Pass => pass_count += 1,
            Status::Fail => fail_count += 1,
            Status::Skip => skip_count += 1,
        }
    }

    snap.push_str(&format!(
        "**{pass_count}** parity_success, **{fail_count}** parity_failure, **{skip_count}** skipped\n\n"
    ));

    snap.push_str("## Failed\n\n");
    for r in results {
        if matches!(r.status, Status::Fail) {
            let msg = r.message.as_deref().unwrap_or("");
            snap.push_str(&format!("- `{}`: {msg}\n", r.name));
        }
    }

    snap.push_str("\n## Passed\n\n");
    for r in results {
        if matches!(r.status, Status::Pass) {
            snap.push_str(&format!("- `{}`\n", r.name));
        }
    }

    snap
}
